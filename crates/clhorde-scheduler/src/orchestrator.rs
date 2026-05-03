//! Glue layer between filesystem events, persisted workflow state, and the
//! daemon.
//!
//! The orchestrator owns:
//! - the in-memory workflow map plus the [`WorkflowStore`] on disk;
//! - per-workflow runtime state (DAG + dispatched-node bookkeeping) used to
//!   drive the apply/verify/archive phases;
//! - an outbound [`ClientRequest`] channel that the binary forwards to the
//!   daemon, and an inbound [`DaemonEvent`] handler that reacts to
//!   `PromptAdded` / `WorkerFinished`.
//!
//! Workflows are advanced via [`Orchestrator::try_advance`]. It is
//! idempotent: every external trigger (FS event, daemon event, manual call)
//! ends in `try_advance(name)` so the scheduler stays in lockstep with disk
//! and daemon state without bespoke per-trigger logic.
//!
//! Tag-based correlation: every prompt the scheduler submits carries a tag
//! of the form `clhorde-scheduler/wf=<name>/phase=<phase>[/node=<id>]`.
//! When `PromptAdded` arrives, the orchestrator inspects `tags` to learn
//! which (workflow, phase, node) the daemon-assigned prompt id belongs to.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use clhorde_core::protocol::{ClientRequest, DaemonEvent, PromptInfo};
use tokio::sync::{broadcast, mpsc};

use crate::control::{DetailNode, SchedulerEvent, WorkflowDetail, WorkflowSummary};
use crate::deps::{self, DepEvaluation};
use crate::dispatch::{is_node_done, next_runnable_nodes};
use crate::openspec::affected_changes::{self, ChangesSnapshot};
use crate::openspec::annotations::{annotate, AnnotatedSection};
use crate::openspec::dag::{self, Dag};
use crate::openspec::discovery::{self, ChangeStatus, MarkerMetadata};
use crate::openspec::tasks_parser;
use crate::persistence::{StoreError, WorkflowStore};
use crate::templates::{self, TemplateEngine};
use crate::watcher::FsEvent;
use crate::workflow::{Workflow, WorkflowStatus};

/// Tag prefix shared by every scheduler-submitted prompt. The full tag is
/// e.g. `clhorde-scheduler/wf=add-oauth/phase=apply/node=1.2`.
const TAG_PREFIX: &str = "clhorde-scheduler";

/// Default mode for scheduler-submitted prompts. The scheduler always uses
/// one-shot so it can react to a clean exit signal; interactive PTY prompts
/// don't fit the unattended dispatch loop.
const PROMPT_MODE: &str = "oneshot";

/// Annotation key written on every prompt the scheduler observed run, with
/// the sorted list of `openspec/changes/<X>/` directories whose content
/// differed between [`DaemonEvent::WorkerStarted`] and
/// [`DaemonEvent::WorkerFinished`]. Always written when a baseline was
/// captured — an empty list signals "we watched, nothing changed".
const AFFECTED_CHANGES_KEY: &str = "openspec.affected_changes";

/// Broadcast capacity for [`SchedulerEvent`]s pushed to subscribed
/// control-socket clients. Big enough that a transiently slow client
/// (e.g. terminal repaints during a `tasks.md` storm) can catch up
/// without taking a `RecvError::Lagged`; small enough that a
/// permanently-stuck client doesn't pin unbounded memory.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Errors surfaced from the orchestrator. We deliberately keep this small —
/// most callers want to log-and-continue, not to branch on the cause.
///
/// The `NotFound` / `BadRequest` / `Io` / `Render` variants only show up on
/// the control-socket entry points (`cancel_workflow`, `retry_section`); the
/// existing FS/event handlers continue to surface only `Store`.
#[derive(Debug)]
pub enum OrchestratorError {
    Store(StoreError),
    NotFound(String),
    BadRequest(String),
    Io(std::io::Error),
    Render(String),
}

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchestratorError::Store(e) => write!(f, "store: {e}"),
            OrchestratorError::NotFound(n) => write!(f, "no such workflow: {n}"),
            OrchestratorError::BadRequest(s) => f.write_str(s),
            OrchestratorError::Io(e) => write!(f, "io: {e}"),
            OrchestratorError::Render(s) => write!(f, "render: {s}"),
        }
    }
}

impl std::error::Error for OrchestratorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OrchestratorError::Store(e) => Some(e),
            OrchestratorError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for OrchestratorError {
    fn from(e: StoreError) -> Self {
        OrchestratorError::Store(e)
    }
}

impl From<std::io::Error> for OrchestratorError {
    fn from(e: std::io::Error) -> Self {
        OrchestratorError::Io(e)
    }
}

/// Captured state used by [`Orchestrator::emit_diff`] to detect
/// genuine state shifts. Pairing the summary and detail snapshots
/// keeps the cost of detail diffing on the same code path as the
/// existing summary diffing — both are rebuilt once before each
/// public mutating method runs and compared once after.
struct StateSnapshot {
    summaries: HashMap<String, WorkflowSummary>,
    details: HashMap<String, WorkflowDetail>,
}

/// Per-prompt dispatch bookkeeping.
#[derive(Debug, Clone, Default, PartialEq)]
struct NodeDispatch {
    /// Tag we set on submit, used to correlate against `PromptAdded`.
    tag: String,
    /// Daemon-assigned numeric prompt id. None until `PromptAdded` lands.
    prompt_id: Option<usize>,
    /// Daemon-assigned UUID. None until `PromptAdded` lands.
    uuid: Option<String>,
    /// Whether `WorkerFinished` arrived for this prompt.
    finished: bool,
    /// Exit code from `WorkerFinished`, if any.
    exit_code: Option<i32>,
    /// Set true once the orchestrator concluded the node is "done" via
    /// `tasks.md`. A finished worker that didn't tick boxes leaves this
    /// false and triggers a workflow failure.
    completed: bool,
}

/// Per-workflow runtime state, kept in memory only. Lost across restarts —
/// for now an in-flight workflow that survives a scheduler crash is
/// re-dispatched from the next runnable node when reconcile sees the marker
/// still present. (Re-dispatch is *idempotent* from the user's view: tasks
/// already ticked stay ticked, the next prompt picks up.)
#[derive(Debug, Clone, Default)]
struct WorkflowRuntime {
    /// Apply-phase DAG, present once we've parsed `tasks.md` at least once.
    dag: Option<Dag>,
    /// Apply-phase dispatched nodes, indexed by DAG node index.
    apply: HashMap<usize, NodeDispatch>,
    /// Verify-phase prompt, if dispatched.
    verify: Option<NodeDispatch>,
    /// Archive-phase prompt, if dispatched.
    archive: Option<NodeDispatch>,
}

pub struct Orchestrator {
    root: PathBuf,
    store: WorkflowStore,
    workflows: BTreeMap<String, Workflow>,
    parsed_tasks: BTreeMap<String, Vec<AnnotatedSection>>,
    runtimes: BTreeMap<String, WorkflowRuntime>,
    templates: TemplateEngine,
    outbound: mpsc::UnboundedSender<ClientRequest>,
    /// Effective working directory per prompt id, learned from
    /// `PromptAdded` / `PromptUpdated`. Prefers `worktree_path` over `cwd`
    /// because the worker's edits land in the worktree, not the original
    /// repo. Used solely to take `openspec/changes/` snapshots.
    prompt_cwds: HashMap<usize, PathBuf>,
    /// Baseline snapshot captured at `WorkerStarted`. Cleared on
    /// `WorkerFinished` after the diff is emitted (or on `PromptRemoved`).
    prompt_baselines: HashMap<usize, ChangesSnapshot>,
    /// Broadcast surface for push-based [`SchedulerEvent`]s.
    /// Subscribers get a fresh receiver via [`Orchestrator::events_subscribe`];
    /// `send` is a no-op when nobody's listening (cheap by design).
    events: broadcast::Sender<SchedulerEvent>,
    /// Detail-event broadcast (`DetailUpdated`). Separate channel from
    /// `events` so a subscriber that only cares about summaries
    /// doesn't take a `Lagged` from a workflow whose nodes flip state
    /// rapidly. Carries every workflow's detail; the control server
    /// filters by name on `SubscribeDetail` connections. `send` is a
    /// no-op when nobody's listening.
    detail_events: broadcast::Sender<SchedulerEvent>,
}

impl Orchestrator {
    pub fn new(
        root: impl Into<PathBuf>,
        store: WorkflowStore,
        outbound: mpsc::UnboundedSender<ClientRequest>,
    ) -> Self {
        let root = root.into();
        let templates = TemplateEngine::new(&root);
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (detail_events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            root,
            store,
            workflows: BTreeMap::new(),
            parsed_tasks: BTreeMap::new(),
            runtimes: BTreeMap::new(),
            templates,
            outbound,
            prompt_cwds: HashMap::new(),
            prompt_baselines: HashMap::new(),
            events,
            detail_events,
        }
    }

    /// Subscribe to push-based summary-level [`SchedulerEvent`]s
    /// (`Snapshot` / `WorkflowUpdated`). Returns a fresh
    /// [`broadcast::Receiver`] that observes events emitted *after*
    /// the call. Callers that need an initial snapshot should pair
    /// this with [`Orchestrator::summaries`] before the first `recv`.
    pub fn events_subscribe(&self) -> broadcast::Receiver<SchedulerEvent> {
        self.events.subscribe()
    }

    /// Subscribe to push-based detail-level [`SchedulerEvent`]s
    /// (`DetailUpdated`). Returns a fresh [`broadcast::Receiver`].
    /// Carries detail events for *every* workflow — callers that
    /// only want one workflow filter by `detail.name`. Pair with
    /// [`Orchestrator::detail`] for the initial snapshot.
    pub fn detail_events_subscribe(&self) -> broadcast::Receiver<SchedulerEvent> {
        self.detail_events.subscribe()
    }

    /// How many subscribers are currently listening on the detail
    /// broadcast channel. Used by tests as a synchronization barrier
    /// — production code shouldn't branch on this value.
    pub fn detail_event_subscriber_count(&self) -> usize {
        self.detail_events.receiver_count()
    }

    /// Snapshot the orchestrator's externally-visible state, paired
    /// across the two broadcast surfaces. Used by the emit-diff
    /// helpers below to detect which workflows actually changed
    /// across a public method call so we only emit one
    /// [`SchedulerEvent::WorkflowUpdated`] / [`SchedulerEvent::DetailUpdated`]
    /// per genuine state shift.
    fn snapshot_state(&self) -> StateSnapshot {
        let summaries = self
            .workflows
            .iter()
            .map(|(name, wf)| (name.clone(), workflow_summary(wf, &self.workflows)))
            .collect();
        let details = self
            .workflows
            .keys()
            .filter_map(|name| self.detail(name).map(|d| (name.clone(), d)))
            .collect();
        StateSnapshot { summaries, details }
    }

    /// Emit one [`SchedulerEvent::WorkflowUpdated`] per workflow
    /// whose [`WorkflowSummary`] differs from `before`, and one
    /// [`SchedulerEvent::DetailUpdated`] (on the dedicated detail
    /// channel) per workflow whose [`WorkflowDetail`] differs.
    /// New workflows (absent from `before`) emit too on both
    /// channels. Removed workflows are not signalled — workflows
    /// currently never disappear from the orchestrator's
    /// `workflows` map (terminal states stay around for the user
    /// to inspect). If that ever changes we'll add explicit
    /// `WorkflowRemoved` / `DetailRemoved` events.
    ///
    /// The two diffs are independent: a `WorkerFinished` on one apply
    /// node can shift `WorkflowDetail` (per-node `state` / `exit_code`)
    /// without changing `WorkflowSummary` (status still
    /// `implementing`, `prompt_ids` unchanged).
    fn emit_diff(&self, before: &StateSnapshot) {
        for wf in self.workflows.values() {
            let summary = workflow_summary(wf, &self.workflows);
            if before.summaries.get(&wf.name) != Some(&summary) {
                let _ = self
                    .events
                    .send(SchedulerEvent::WorkflowUpdated { summary });
            }
            if let Some(detail) = self.detail(&wf.name) {
                if before.details.get(&wf.name) != Some(&detail) {
                    let _ = self
                        .detail_events
                        .send(SchedulerEvent::DetailUpdated { detail });
                }
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reconcile in-memory state with what's on disk. Idempotent — call at
    /// startup *and* whenever the FS or the store may have drifted.
    pub fn reconcile(&mut self) -> Result<(), OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.reconcile_inner();
        self.emit_diff(&before);
        result
    }

    fn reconcile_inner(&mut self) -> Result<(), OrchestratorError> {
        for wf in self.store.list()? {
            self.workflows.insert(wf.name.clone(), wf);
        }

        let discovered = discovery::scan(&self.root);
        for change in &discovered {
            self.reconcile_change(change)?;
            self.refresh_tasks(&change.name);
        }

        // After the FS pass, kick the state machine on every queued or
        // running workflow so prompts get dispatched if the daemon is up.
        let names: Vec<String> = self.workflows.keys().cloned().collect();
        for n in names {
            if let Err(e) = self.try_advance_inner(&n) {
                tracing::warn!(name = %n, error = %e, "reconcile try_advance");
            }
        }
        Ok(())
    }

    pub fn workflows(&self) -> impl Iterator<Item = &Workflow> {
        self.workflows.values()
    }

    pub fn workflow(&self, name: &str) -> Option<&Workflow> {
        self.workflows.get(name)
    }

    pub fn tasks_for(&self, name: &str) -> &[AnnotatedSection] {
        self.parsed_tasks
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ── control-socket entry points ──

    /// Snapshot every workflow as a [`WorkflowSummary`]. Used by the
    /// scheduler control socket to answer `Status { name: None }`.
    pub fn summaries(&self) -> Vec<WorkflowSummary> {
        self.workflows
            .values()
            .map(|wf| workflow_summary(wf, &self.workflows))
            .collect()
    }

    /// Snapshot one workflow as a [`WorkflowSummary`], or `None` if it
    /// does not exist.
    pub fn summary(&self, name: &str) -> Option<WorkflowSummary> {
        self.workflows
            .get(name)
            .map(|wf| workflow_summary(wf, &self.workflows))
    }

    /// Assemble a [`WorkflowDetail`] for `name`, or `None` if no
    /// workflow with that name exists. The detail merges the persisted
    /// `Workflow` (status + timestamps) with the in-memory runtime
    /// (per-node dispatch state + DAG ordering). Workflows without a
    /// runtime (Drafted, or Queued before tasks.md was first parsed)
    /// surface an empty `apply` list — that matches the wire contract
    /// described on `WorkflowDetail::apply`.
    pub fn detail(&self, name: &str) -> Option<WorkflowDetail> {
        let wf = self.workflows.get(name)?;
        let summary = workflow_summary(wf, &self.workflows);
        let runtime = self.runtimes.get(name);
        let apply = runtime
            .and_then(|r| r.dag.as_ref().map(|d| (r, d)))
            .map(|(runtime, dag)| {
                dag.nodes
                    .iter()
                    .enumerate()
                    .map(|(idx, node)| build_apply_detail(node, idx, dag, runtime))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let verify = runtime
            .and_then(|r| r.verify.as_ref())
            .map(|d| build_phase_detail("verify", "Verify phase", d));
        let archive = runtime
            .and_then(|r| r.archive.as_ref())
            .map(|d| build_phase_detail("archive", "Archive phase", d));
        Some(WorkflowDetail {
            name: summary.name,
            status: summary.status,
            failure_reason: summary.failure_reason,
            priority: summary.priority,
            queued_at: summary.queued_at,
            started_at: summary.started_at,
            finished_at: summary.finished_at,
            apply,
            verify,
            archive,
            blocked_by: summary.blocked_by,
        })
    }

    /// Remove the `.clhorde-ready` marker on disk (if present) and
    /// transition the workflow to `Cancelled` (or `Drafted`, if it was
    /// only queued). Equivalent to a watcher seeing the marker disappear,
    /// but driven explicitly so the control socket gets a synchronous
    /// confirmation.
    ///
    /// `kind` semantics in the returned tuple:
    /// - `"unqueued"`: Queued → Drafted.
    /// - `"cancelled"`: Implementing/Verifying/Archiving → Cancelled.
    /// - `"noop"`: terminal or already-Drafted; no state change but the
    ///   marker (if present) was still removed.
    pub fn cancel_workflow(
        &mut self,
        name: &str,
    ) -> Result<&'static str, OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.cancel_workflow_inner(name);
        self.emit_diff(&before);
        result
    }

    fn cancel_workflow_inner(
        &mut self,
        name: &str,
    ) -> Result<&'static str, OrchestratorError> {
        let marker = self
            .root
            .join("openspec")
            .join("changes")
            .join(name)
            .join(".clhorde-ready");
        match fs::remove_file(&marker) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(OrchestratorError::Io(e)),
        }

        let kind = match self.workflows.get(name).map(|w| w.status.clone()) {
            Some(WorkflowStatus::Queued) => "unqueued",
            Some(WorkflowStatus::Implementing)
            | Some(WorkflowStatus::Verifying)
            | Some(WorkflowStatus::Archiving) => "cancelled",
            Some(_) => "noop",
            None => return Err(OrchestratorError::NotFound(name.to_string())),
        };

        // Reuse the existing transition logic. It does the right thing:
        // Queued→Drafted, Implementing/Verifying/Archiving→Cancelled, and
        // saves the result.
        let pre_terminal = self
            .workflows
            .get(name)
            .map(|w| w.status.is_terminal())
            .unwrap_or(true);
        self.on_marker_removed(name.to_string())?;
        let post_terminal = self
            .workflows
            .get(name)
            .map(|w| w.status.is_terminal())
            .unwrap_or(true);
        if !pre_terminal && post_terminal {
            self.cascade_dependents(name);
        }
        Ok(kind)
    }

    /// Queue a draft change by writing
    /// `openspec/changes/<name>/.clhorde-ready` with optional metadata,
    /// then transition the workflow through `on_marker_created` so the
    /// state machine matches what an external `touch` would produce.
    /// Errors if the change directory is missing — without this, a
    /// typo would silently create an orphan marker.
    pub fn queue_workflow(
        &mut self,
        name: &str,
        priority: Option<i32>,
    ) -> Result<(), OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.queue_workflow_inner(name, priority);
        self.emit_diff(&before);
        result
    }

    fn queue_workflow_inner(
        &mut self,
        name: &str,
        priority: Option<i32>,
    ) -> Result<(), OrchestratorError> {
        let change_dir = self
            .root
            .join("openspec")
            .join("changes")
            .join(name);
        if !change_dir.is_dir() {
            return Err(OrchestratorError::NotFound(format!(
                "no such change: {name}"
            )));
        }
        let marker_path = change_dir.join(".clhorde-ready");
        let body = match priority {
            Some(p) => format!("priority = {p}\n"),
            None => String::new(),
        };
        fs::write(&marker_path, body).map_err(OrchestratorError::Io)?;
        self.on_marker_created(name.to_string())
    }

    /// Re-dispatch a single apply-phase node by its `tasks.md` id. If the
    /// workflow is `Failed`, it is reset to `Implementing` so the next
    /// `WorkerFinished` advances it. The dispatch goes through the usual
    /// outbound channel — i.e. it lands in the daemon as if the
    /// orchestrator had decided to dispatch it.
    pub fn retry_section(
        &mut self,
        name: &str,
        section_id: &str,
    ) -> Result<(), OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.retry_section_inner(name, section_id);
        self.emit_diff(&before);
        result
    }

    fn retry_section_inner(
        &mut self,
        name: &str,
        section_id: &str,
    ) -> Result<(), OrchestratorError> {
        // Load + ensure the workflow is in a state we can retry from.
        let mut wf = self
            .workflows
            .remove(name)
            .ok_or_else(|| OrchestratorError::NotFound(name.to_string()))?;
        match wf.status {
            WorkflowStatus::Failed { .. } => {
                wf.status = WorkflowStatus::Implementing;
            }
            WorkflowStatus::Archived | WorkflowStatus::Cancelled => {
                self.workflows.insert(name.to_string(), wf);
                return Err(OrchestratorError::BadRequest(format!(
                    "{name}: cannot retry a terminal workflow"
                )));
            }
            _ => {}
        }

        // Re-parse `tasks.md` and rebuild the DAG. We do not assume the
        // in-memory parsed_tasks is fresh; the user may have edited the
        // file between the failure and the retry.
        self.refresh_tasks(name);
        let sections = match self.parsed_tasks.get(name).cloned() {
            Some(s) => s,
            None => {
                self.workflows.insert(name.to_string(), wf);
                return Err(OrchestratorError::BadRequest(format!(
                    "{name}: tasks.md missing or unreadable"
                )));
            }
        };
        let built_dag = match dag::build(&sections) {
            Ok(d) => d,
            Err(e) => {
                self.workflows.insert(name.to_string(), wf);
                return Err(OrchestratorError::BadRequest(format!(
                    "{name}: dag: {e}"
                )));
            }
        };
        let node = match built_dag.nodes.iter().find(|n| n.id == section_id) {
            Some(n) => n.clone(),
            None => {
                self.workflows.insert(name.to_string(), wf);
                return Err(OrchestratorError::BadRequest(format!(
                    "{name}: no node {section_id}"
                )));
            }
        };

        let section = sections.iter().find(|s| s.section.id == node.id);
        let tasks_block = section
            .map(|s| {
                s.items
                    .iter()
                    .map(|t| {
                        let mark = if t.task.done { "[x]" } else { "[ ]" };
                        format!("- {} {} {}", mark, t.task.id, t.task.text)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let prompt = self.render_apply(name, &node, &tasks_block);
        let tag = expected_apply_tag(name, &node.id);
        let request = ClientRequest::SubmitPrompt {
            text: prompt,
            cwd: Some(self.root.to_string_lossy().into_owned()),
            mode: PROMPT_MODE.to_string(),
            worktree: true,
            tags: vec![tag.clone()],
            depends_on: Vec::new(),
            worktree_id: Some(name.to_string()),
        };
        if let Err(e) = self.outbound.send(request) {
            tracing::warn!(name, error = %e, "outbound channel closed (retry)");
        } else {
            // Refresh the runtime so `note_prompt` finds an entry to fill
            // when `PromptAdded` arrives. Without this, retries from a
            // clean restart (no in-memory runtime) would miss the
            // correlation.
            let dag_clone = built_dag.clone();
            let runtime = self.runtimes.entry(name.to_string()).or_default();
            runtime.dag.get_or_insert(dag_clone);
            if let Some(idx) = built_dag
                .nodes
                .iter()
                .position(|n| n.id == node.id)
            {
                runtime.apply.insert(
                    idx,
                    NodeDispatch {
                        tag,
                        ..NodeDispatch::default()
                    },
                );
            }
        }
        if let Err(e) = self.store.save(&wf) {
            tracing::warn!(name, error = %e, "persist after retry failed");
        }
        self.workflows.insert(name.to_string(), wf);
        Ok(())
    }

    /// Process one [`FsEvent`].
    pub fn handle_event(&mut self, event: FsEvent) -> Result<(), OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.handle_event_inner(event);
        self.emit_diff(&before);
        result
    }

    fn handle_event_inner(
        &mut self,
        event: FsEvent,
    ) -> Result<(), OrchestratorError> {
        let name = match &event {
            FsEvent::MarkerCreated { name } => name.clone(),
            FsEvent::MarkerRemoved { name } => name.clone(),
            FsEvent::TasksModified { name } => name.clone(),
        };
        match event {
            FsEvent::MarkerCreated { name } => self.on_marker_created(name)?,
            FsEvent::MarkerRemoved { name } => self.on_marker_removed(name)?,
            FsEvent::TasksModified { name } => {
                self.refresh_tasks(&name);
            }
        }
        self.try_advance_inner(&name)
    }

    /// Process one [`DaemonEvent`] from the upstream daemon. Only the
    /// events the scheduler cares about are handled — everything else is a
    /// no-op so unrelated traffic doesn't pollute the workflow state.
    pub fn handle_daemon_event(
        &mut self,
        event: &DaemonEvent,
    ) -> Result<(), OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.handle_daemon_event_inner(event);
        self.emit_diff(&before);
        result
    }

    fn handle_daemon_event_inner(
        &mut self,
        event: &DaemonEvent,
    ) -> Result<(), OrchestratorError> {
        match event {
            DaemonEvent::PromptAdded(info) | DaemonEvent::PromptUpdated(info) => {
                self.note_prompt_cwd(info);
                self.note_prompt(info);
            }
            DaemonEvent::StateSnapshot(state) => {
                // After a reconnect we may have missed a string of events;
                // populate the cwd map from the snapshot so future
                // `WorkerStarted`s have a path to fall back on. We do *not*
                // backfill `prompt_baselines` for already-running prompts —
                // a baseline taken mid-run would be misleading.
                for info in &state.prompts {
                    self.note_prompt_cwd(info);
                    self.note_prompt(info);
                }
            }
            DaemonEvent::WorkerStarted { prompt_id } => {
                self.capture_baseline(*prompt_id);
            }
            DaemonEvent::WorkerFinished {
                prompt_id,
                exit_code,
            } => {
                self.emit_affected_changes(*prompt_id);
                if let Some(name) = self.workflow_owning_prompt(*prompt_id) {
                    self.note_worker_finished(&name, *prompt_id, *exit_code);
                    self.try_advance_inner(&name)?;
                }
            }
            DaemonEvent::PromptRemoved { prompt_id } => {
                self.prompt_cwds.remove(prompt_id);
                self.prompt_baselines.remove(prompt_id);
            }
            _ => {}
        }
        Ok(())
    }

    /// Idempotent kick on a single workflow's state machine. Safe to call
    /// from any external trigger. Internal callers in this file use
    /// [`Orchestrator::try_advance_inner`] directly to avoid emitting
    /// duplicate [`SchedulerEvent`]s when the surrounding public method
    /// already wraps the change with [`Orchestrator::emit_diff`].
    pub fn try_advance(&mut self, name: &str) -> Result<(), OrchestratorError> {
        let before = self.snapshot_state();
        let result = self.try_advance_inner(name);
        self.emit_diff(&before);
        result
    }

    fn try_advance_inner(&mut self, name: &str) -> Result<(), OrchestratorError> {
        let Some(mut wf) = self.workflows.remove(name) else {
            return Ok(());
        };
        let pre_terminal = wf.status.is_terminal();
        let result = self.advance_inner(&mut wf);
        // Save and reinsert regardless of error so we don't lose the
        // workflow on a transient failure.
        if let Err(e) = self.store.save(&wf) {
            tracing::warn!(name = %wf.name, error = %e, "persisting after advance failed");
        }
        let post_terminal = wf.status.is_terminal();
        self.workflows.insert(name.to_string(), wf);

        // Inter-workflow cascade: a freshly-terminal workflow may unblock
        // (Archived) or fail (Cancelled/Failed) any dependent in `Queued`.
        // Bounded recursion: each invocation drops one workflow into a
        // terminal state; with cycle detection rejecting cycles, the dep
        // graph is a DAG and recursion depth = depth of dependents.
        if !pre_terminal && post_terminal {
            self.cascade_dependents(name);
        }
        result
    }

    /// Find every workflow that lists `name` in its `depends_on` and
    /// re-evaluate it via [`Self::try_advance_inner`]. Called after a
    /// state transition that may have unblocked or failed a dependent.
    fn cascade_dependents(&mut self, name: &str) {
        let dependents: Vec<String> = self
            .workflows
            .iter()
            .filter(|(n, w)| {
                n.as_str() != name
                    && w.metadata.depends_on.iter().any(|d| d == name)
            })
            .map(|(n, _)| n.clone())
            .collect();
        for dep_name in dependents {
            if let Err(e) = self.try_advance_inner(&dep_name) {
                tracing::warn!(
                    name = %dep_name,
                    parent = %name,
                    error = %e,
                    "cascade try_advance failed"
                );
            }
        }
    }

    // ── advance phases ──

    fn advance_inner(&mut self, wf: &mut Workflow) -> Result<(), OrchestratorError> {
        match wf.status {
            WorkflowStatus::Queued => self.advance_queued(wf),
            WorkflowStatus::Implementing => self.advance_implementing(wf),
            WorkflowStatus::Verifying => self.advance_verifying(wf),
            WorkflowStatus::Archiving => self.advance_archiving(wf),
            // Drafted: nothing to dispatch.
            // Archived/Cancelled/Failed: terminal, nothing to do.
            _ => Ok(()),
        }
    }

    fn advance_queued(&mut self, wf: &mut Workflow) -> Result<(), OrchestratorError> {
        let Some(sections) = self.parsed_tasks.get(&wf.name).cloned() else {
            return Ok(()); // wait for `tasks.md`
        };
        if sections.is_empty() {
            return Ok(());
        }
        // Inter-workflow gate: hold or fail per `depends_on` in the marker.
        // `wf` is owned here (already removed from `self.workflows` by
        // `try_advance_inner`) so passing `&self.workflows` is fine.
        match deps::evaluate(wf, &self.workflows) {
            DepEvaluation::Satisfied => {}
            DepEvaluation::Pending(_) => return Ok(()),
            DepEvaluation::Failed(reason) => {
                let _ = wf.fail(reason);
                return Ok(());
            }
        }
        let dag = match dag::build(&sections) {
            Ok(d) => d,
            Err(e) => {
                let _ = wf.fail(format!("dag build: {e}"));
                return Ok(());
            }
        };
        if wf.start_implementing().is_err() {
            return Ok(());
        }
        let runtime = self.runtimes.entry(wf.name.clone()).or_default();
        runtime.dag = Some(dag.clone());
        self.dispatch_apply(wf, &dag, &sections);
        Ok(())
    }

    fn advance_implementing(
        &mut self,
        wf: &mut Workflow,
    ) -> Result<(), OrchestratorError> {
        let Some(sections) = self.parsed_tasks.get(&wf.name).cloned() else {
            return Ok(());
        };
        let Some(dag) = self
            .runtimes
            .get(&wf.name)
            .and_then(|r| r.dag.clone())
        else {
            return Ok(());
        };

        // Sweep finished nodes, marking them complete or failing the workflow.
        let mut runtime_failed: Option<String> = None;
        if let Some(runtime) = self.runtimes.get_mut(&wf.name) {
            for (idx, dispatch) in runtime.apply.iter_mut() {
                if !dispatch.finished || dispatch.completed {
                    continue;
                }
                let exit = dispatch.exit_code.unwrap_or(0);
                let node_done = is_node_done(&dag, *idx, &sections);
                if exit == 0 && node_done {
                    dispatch.completed = true;
                } else if exit != 0 {
                    runtime_failed = Some(format!(
                        "apply node {} exited with code {}",
                        dag.nodes[*idx].id, exit
                    ));
                    break;
                } else {
                    runtime_failed = Some(format!(
                        "apply node {} exited 0 but tasks remain unchecked",
                        dag.nodes[*idx].id
                    ));
                    break;
                }
            }
        }
        if let Some(reason) = runtime_failed {
            let _ = wf.fail(reason);
            return Ok(());
        }

        if all_apply_complete(&self.runtimes[&wf.name], &dag) {
            if wf.start_verifying().is_ok() {
                self.dispatch_verify(wf);
            }
            return Ok(());
        }

        self.dispatch_apply(wf, &dag, &sections);
        Ok(())
    }

    fn advance_verifying(
        &mut self,
        wf: &mut Workflow,
    ) -> Result<(), OrchestratorError> {
        let runtime = self.runtimes.entry(wf.name.clone()).or_default();
        let verify = runtime.verify.as_ref().cloned();
        match verify {
            None => {
                self.dispatch_verify(wf);
            }
            Some(d) if d.finished => {
                let exit = d.exit_code.unwrap_or(0);
                if exit == 0 {
                    if wf.start_archiving().is_ok() {
                        self.dispatch_archive(wf);
                    }
                } else {
                    let _ = wf.fail(format!("verify exited with code {exit}"));
                }
            }
            Some(_) => {} // dispatched, not yet finished
        }
        Ok(())
    }

    fn advance_archiving(
        &mut self,
        wf: &mut Workflow,
    ) -> Result<(), OrchestratorError> {
        let runtime = self.runtimes.entry(wf.name.clone()).or_default();
        let archive = runtime.archive.as_ref().cloned();
        match archive {
            None => {
                self.dispatch_archive(wf);
            }
            Some(d) if d.finished => {
                let exit = d.exit_code.unwrap_or(0);
                if exit == 0 {
                    let _ = wf.archive();
                } else {
                    let _ = wf.fail(format!("archive exited with code {exit}"));
                }
            }
            Some(_) => {}
        }
        Ok(())
    }

    // ── dispatch ──

    fn dispatch_apply(
        &mut self,
        wf: &mut Workflow,
        dag: &Dag,
        sections: &[AnnotatedSection],
    ) {
        let (completed, dispatched) = self.apply_state(&wf.name);
        for idx in next_runnable_nodes(dag, &completed, &dispatched) {
            let node = &dag.nodes[idx];
            let section = sections.iter().find(|s| s.section.id == node.id);
            let tasks_block = section
                .map(|s| {
                    s.items
                        .iter()
                        .map(|t| {
                            let mark = if t.task.done { "[x]" } else { "[ ]" };
                            format!("- {} {} {}", mark, t.task.id, t.task.text)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();

            let prompt = self.render_apply(&wf.name, node, &tasks_block);
            let tag = format!(
                "{}/wf={}/phase=apply/node={}",
                TAG_PREFIX, wf.name, node.id
            );

            // depends_on: prompt ids of predecessors that have arrived.
            let deps_ids: Vec<usize> = node
                .deps
                .iter()
                .filter_map(|d| self.runtimes.get(&wf.name)?.apply.get(d)?.prompt_id)
                .collect();

            let request = ClientRequest::SubmitPrompt {
                text: prompt,
                cwd: Some(self.root.to_string_lossy().into_owned()),
                mode: PROMPT_MODE.to_string(),
                worktree: true,
                tags: vec![tag.clone()],
                depends_on: deps_ids,
                worktree_id: Some(wf.name.clone()),
            };
            if let Err(e) = self.outbound.send(request) {
                tracing::warn!(name = %wf.name, error = %e, "outbound channel closed");
                return;
            }

            let runtime = self.runtimes.entry(wf.name.clone()).or_default();
            runtime.apply.insert(
                idx,
                NodeDispatch {
                    tag,
                    ..NodeDispatch::default()
                },
            );
        }
    }

    fn dispatch_verify(&mut self, wf: &mut Workflow) {
        let prompt = self.render_verify(&wf.name);
        let tag = format!("{}/wf={}/phase=verify", TAG_PREFIX, wf.name);
        let request = ClientRequest::SubmitPrompt {
            text: prompt,
            cwd: Some(self.root.to_string_lossy().into_owned()),
            mode: PROMPT_MODE.to_string(),
            worktree: true,
            tags: vec![tag.clone()],
            depends_on: Vec::new(),
            worktree_id: Some(wf.name.clone()),
        };
        if let Err(e) = self.outbound.send(request) {
            tracing::warn!(name = %wf.name, error = %e, "outbound channel closed (verify)");
            return;
        }
        let runtime = self.runtimes.entry(wf.name.clone()).or_default();
        runtime.verify = Some(NodeDispatch {
            tag,
            ..NodeDispatch::default()
        });
    }

    fn dispatch_archive(&mut self, wf: &mut Workflow) {
        let prompt = self.render_archive(&wf.name);
        let tag = format!("{}/wf={}/phase=archive", TAG_PREFIX, wf.name);
        let request = ClientRequest::SubmitPrompt {
            text: prompt,
            cwd: Some(self.root.to_string_lossy().into_owned()),
            mode: PROMPT_MODE.to_string(),
            worktree: true,
            tags: vec![tag.clone()],
            depends_on: Vec::new(),
            worktree_id: Some(wf.name.clone()),
        };
        if let Err(e) = self.outbound.send(request) {
            tracing::warn!(name = %wf.name, error = %e, "outbound channel closed (archive)");
            return;
        }
        let runtime = self.runtimes.entry(wf.name.clone()).or_default();
        runtime.archive = Some(NodeDispatch {
            tag,
            ..NodeDispatch::default()
        });
    }

    fn render_apply(
        &self,
        change_name: &str,
        node: &crate::openspec::dag::DagNode,
        tasks_block: &str,
    ) -> String {
        let mut ctx = tera::Context::new();
        ctx.insert("change_name", change_name);
        ctx.insert(
            "change_dir",
            &format!(
                "{}/openspec/changes/{}",
                self.root.to_string_lossy(),
                change_name
            ),
        );
        ctx.insert("section_id", &node.id);
        ctx.insert("section_title", &node.label);
        ctx.insert("tasks_block", tasks_block);
        let template = node.prompt_template.as_deref().unwrap_or(templates::APPLY_SECTION);
        match self.templates.render(template, &ctx) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(template, error = %e, "apply template render failed");
                format!("[apply {}/{}]", change_name, node.id)
            }
        }
    }

    fn render_verify(&self, change_name: &str) -> String {
        let mut ctx = tera::Context::new();
        ctx.insert("change_name", change_name);
        ctx.insert(
            "change_dir",
            &format!(
                "{}/openspec/changes/{}",
                self.root.to_string_lossy(),
                change_name
            ),
        );
        match self.templates.render(templates::VERIFY, &ctx) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "verify template render failed");
                format!("[verify {change_name}]")
            }
        }
    }

    fn render_archive(&self, change_name: &str) -> String {
        let mut ctx = tera::Context::new();
        ctx.insert("change_name", change_name);
        match self.templates.render(templates::ARCHIVE, &ctx) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "archive template render failed");
                format!("[archive {change_name}]")
            }
        }
    }

    // ── daemon-event helpers ──

    /// Update [`Self::prompt_cwds`] from a freshly-seen `PromptInfo`. The
    /// effective cwd is `worktree_path` when the prompt runs in a worktree,
    /// otherwise the original `cwd`. Prompts with neither are skipped —
    /// `affected_changes` requires a directory to snapshot.
    fn note_prompt_cwd(&mut self, info: &PromptInfo) {
        let path = info
            .worktree_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(info.cwd.as_deref())
            .map(PathBuf::from);
        if let Some(p) = path {
            self.prompt_cwds.insert(info.id, p);
        }
    }

    /// Take the baseline `openspec/changes/` snapshot for a prompt that
    /// just started. Silently no-ops if we have no cwd for it (the
    /// scheduler missed `PromptAdded`, e.g. it joined mid-flight).
    fn capture_baseline(&mut self, prompt_id: usize) {
        let Some(cwd) = self.prompt_cwds.get(&prompt_id).cloned() else {
            return;
        };
        let snap = affected_changes::snapshot(&cwd);
        self.prompt_baselines.insert(prompt_id, snap);
    }

    /// Diff the post-finish snapshot against the baseline and write the
    /// `openspec.affected_changes` annotation. Always emits when a baseline
    /// existed — an empty list means "watched, nothing changed", which is
    /// distinguishable from a missing key.
    fn emit_affected_changes(&mut self, prompt_id: usize) {
        let Some(cwd) = self.prompt_cwds.get(&prompt_id).cloned() else {
            return;
        };
        let Some(before) = self.prompt_baselines.remove(&prompt_id) else {
            return;
        };
        let after = affected_changes::snapshot(&cwd);
        let affected = affected_changes::diff(&before, &after);
        let request = ClientRequest::SetAnnotation {
            prompt_id,
            key: AFFECTED_CHANGES_KEY.to_string(),
            value: serde_json::json!(affected),
        };
        if let Err(e) = self.outbound.send(request) {
            tracing::warn!(
                prompt_id,
                error = %e,
                "could not send openspec.affected_changes annotation"
            );
        }
    }

    fn note_prompt(&mut self, info: &PromptInfo) {
        let Some((name, target)) = parse_scheduler_tag(&info.tags) else {
            return;
        };
        let runtime = self.runtimes.entry(name.clone()).or_default();
        match target {
            TagTarget::Apply { node_id } => {
                if let Some(dag) = runtime.dag.as_ref() {
                    if let Some(idx) = dag.nodes.iter().position(|n| n.id == node_id) {
                        let entry = runtime
                            .apply
                            .entry(idx)
                            .or_default();
                        entry.prompt_id = Some(info.id);
                        entry.uuid = Some(info.uuid.clone());
                        if entry.tag.is_empty() {
                            entry.tag = expected_apply_tag(&name, &node_id);
                        }
                        if !info.depends_on.is_empty() {
                            // Persist propagation of resolved deps onto the workflow's
                            // canonical prompt list — not strictly needed but useful
                            // for inspection.
                            if let Some(wf) = self.workflows.get_mut(&name) {
                                if !wf.prompt_ids.contains(&info.uuid) {
                                    wf.prompt_ids.push(info.uuid.clone());
                                }
                            }
                        } else if let Some(wf) = self.workflows.get_mut(&name) {
                            if !wf.prompt_ids.contains(&info.uuid) {
                                wf.prompt_ids.push(info.uuid.clone());
                            }
                        }
                    }
                }
            }
            TagTarget::Verify => {
                let entry = runtime.verify.get_or_insert_with(NodeDispatch::default);
                entry.prompt_id = Some(info.id);
                entry.uuid = Some(info.uuid.clone());
                if entry.tag.is_empty() {
                    entry.tag = format!("{TAG_PREFIX}/wf={name}/phase=verify");
                }
            }
            TagTarget::Archive => {
                let entry = runtime.archive.get_or_insert_with(NodeDispatch::default);
                entry.prompt_id = Some(info.id);
                entry.uuid = Some(info.uuid.clone());
                if entry.tag.is_empty() {
                    entry.tag = format!("{TAG_PREFIX}/wf={name}/phase=archive");
                }
            }
        }
    }

    fn note_worker_finished(
        &mut self,
        name: &str,
        prompt_id: usize,
        exit_code: Option<i32>,
    ) {
        let Some(runtime) = self.runtimes.get_mut(name) else {
            return;
        };
        for d in runtime.apply.values_mut() {
            if d.prompt_id == Some(prompt_id) {
                d.finished = true;
                d.exit_code = exit_code;
                return;
            }
        }
        if let Some(d) = runtime.verify.as_mut() {
            if d.prompt_id == Some(prompt_id) {
                d.finished = true;
                d.exit_code = exit_code;
                return;
            }
        }
        if let Some(d) = runtime.archive.as_mut() {
            if d.prompt_id == Some(prompt_id) {
                d.finished = true;
                d.exit_code = exit_code;
            }
        }
    }

    fn workflow_owning_prompt(&self, prompt_id: usize) -> Option<String> {
        for (name, runtime) in &self.runtimes {
            if runtime
                .apply
                .values()
                .any(|d| d.prompt_id == Some(prompt_id))
            {
                return Some(name.clone());
            }
            if runtime
                .verify
                .as_ref()
                .is_some_and(|d| d.prompt_id == Some(prompt_id))
            {
                return Some(name.clone());
            }
            if runtime
                .archive
                .as_ref()
                .is_some_and(|d| d.prompt_id == Some(prompt_id))
            {
                return Some(name.clone());
            }
        }
        None
    }

    fn apply_state(&self, name: &str) -> (HashSet<usize>, HashSet<usize>) {
        let Some(runtime) = self.runtimes.get(name) else {
            return (HashSet::new(), HashSet::new());
        };
        let mut completed = HashSet::new();
        let mut dispatched = HashSet::new();
        for (idx, d) in &runtime.apply {
            dispatched.insert(*idx);
            if d.completed {
                completed.insert(*idx);
            }
        }
        (completed, dispatched)
    }

    // ── private (FS reconciliation) ──

    fn reconcile_change(
        &mut self,
        change: &discovery::DiscoveredChange,
    ) -> Result<(), OrchestratorError> {
        match (&change.status, self.workflows.remove(&change.name)) {
            (ChangeStatus::Queued(meta), None) => {
                let wf = Workflow::queued(&change.name, meta.clone());
                self.store.save(&wf)?;
                self.workflows.insert(change.name.clone(), wf);
            }
            (ChangeStatus::Queued(meta), Some(mut wf)) => {
                let changed = match wf.status {
                    WorkflowStatus::Drafted => {
                        if let Err(e) = wf.queue(meta.clone()) {
                            tracing::warn!(name = %change.name, error = %e, "reconcile queue failed");
                            let _ = wf.fail(format!("reconcile queue: {e}"));
                        }
                        true
                    }
                    WorkflowStatus::Queued => {
                        if wf.metadata != *meta {
                            wf.metadata = meta.clone();
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if changed {
                    self.store.save(&wf)?;
                }
                self.workflows.insert(change.name.clone(), wf);
            }
            (ChangeStatus::Drafted, None) => {
                let wf = Workflow::drafted(&change.name);
                self.store.save(&wf)?;
                self.workflows.insert(change.name.clone(), wf);
            }
            (ChangeStatus::Drafted, Some(mut wf)) => {
                let changed = match wf.status {
                    WorkflowStatus::Queued => wf.unqueue().is_ok(),
                    WorkflowStatus::Implementing
                    | WorkflowStatus::Verifying
                    | WorkflowStatus::Archiving => wf.cancel().is_ok(),
                    _ => false,
                };
                if changed {
                    self.store.save(&wf)?;
                }
                self.workflows.insert(change.name.clone(), wf);
            }
        }
        Ok(())
    }

    fn on_marker_created(&mut self, name: String) -> Result<(), OrchestratorError> {
        let metadata = self.read_marker(&name);
        match self.workflows.remove(&name) {
            Some(mut wf) => {
                let changed = match wf.status {
                    WorkflowStatus::Drafted => {
                        if let Err(e) = wf.queue(metadata) {
                            tracing::warn!(name = %name, error = %e, "queue rejected");
                            false
                        } else {
                            true
                        }
                    }
                    WorkflowStatus::Queued => {
                        if wf.metadata != metadata {
                            wf.metadata = metadata;
                            true
                        } else {
                            false
                        }
                    }
                    _ => {
                        tracing::debug!(
                            name = %name,
                            status = ?wf.status,
                            "marker (re)created while non-Drafted; ignoring"
                        );
                        false
                    }
                };
                if changed {
                    self.store.save(&wf)?;
                }
                self.workflows.insert(name, wf);
            }
            None => {
                let wf = Workflow::queued(&name, metadata);
                self.store.save(&wf)?;
                self.workflows.insert(name, wf);
            }
        }
        Ok(())
    }

    fn on_marker_removed(&mut self, name: String) -> Result<(), OrchestratorError> {
        if let Some(mut wf) = self.workflows.remove(&name) {
            let changed = match wf.status {
                WorkflowStatus::Queued => wf.unqueue().is_ok(),
                WorkflowStatus::Implementing
                | WorkflowStatus::Verifying
                | WorkflowStatus::Archiving => wf.cancel().is_ok(),
                _ => false,
            };
            if changed {
                self.store.save(&wf)?;
            }
            self.workflows.insert(name, wf);
        }
        Ok(())
    }

    fn refresh_tasks(&mut self, name: &str) {
        let path = self
            .root
            .join("openspec")
            .join("changes")
            .join(name)
            .join("tasks.md");
        let body = match fs::read_to_string(&path) {
            Ok(b) => b,
            Err(_) => {
                self.parsed_tasks.remove(name);
                return;
            }
        };
        let graph = tasks_parser::parse(&body);
        let annotated = annotate(graph);
        self.parsed_tasks.insert(name.to_string(), annotated);
    }

    fn read_marker(&self, name: &str) -> MarkerMetadata {
        let path = self
            .root
            .join("openspec")
            .join("changes")
            .join(name)
            .join(".clhorde-ready");
        match fs::read_to_string(&path) {
            Ok(body) => MarkerMetadata::parse(&body).unwrap_or_else(|e| {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "malformed .clhorde-ready; using defaults"
                );
                MarkerMetadata::default()
            }),
            Err(_) => MarkerMetadata::default(),
        }
    }
}

/// Convert a [`Workflow`] into the wire-format [`WorkflowSummary`] used
/// by the control socket. Pure transformation; lives outside the impl
/// block so tests can call it directly.
///
/// `others` is the orchestrator's view of every other workflow, used to
/// compute the `blocked_by` field via [`deps::evaluate`]. For workflows
/// in any state other than `Queued`, `blocked_by` is always empty.
fn workflow_summary(
    wf: &Workflow,
    others: &BTreeMap<String, Workflow>,
) -> WorkflowSummary {
    let (status, failure_reason) = match &wf.status {
        WorkflowStatus::Drafted => ("drafted", None),
        WorkflowStatus::Queued => ("queued", None),
        WorkflowStatus::Implementing => ("implementing", None),
        WorkflowStatus::Verifying => ("verifying", None),
        WorkflowStatus::Archiving => ("archiving", None),
        WorkflowStatus::Archived => ("archived", None),
        WorkflowStatus::Cancelled => ("cancelled", None),
        WorkflowStatus::Failed { reason } => ("failed", Some(reason.clone())),
    };
    let blocked_by = if matches!(wf.status, WorkflowStatus::Queued) {
        match deps::evaluate(wf, others) {
            DepEvaluation::Pending(names) => names,
            // Satisfied / Failed are not "blocking" states from the
            // user's POV: Satisfied means the workflow will advance on
            // the next try; Failed means the gate already promoted the
            // workflow into `Failed` (so we won't see Queued+Failed at
            // a single instant, but if we did, we'd surface the dep
            // failure via `failure_reason` rather than `blocked_by`).
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    WorkflowSummary {
        name: wf.name.clone(),
        status: status.to_string(),
        failure_reason,
        priority: wf.metadata.priority.unwrap_or(0),
        queued_at: wf.queued_at,
        started_at: wf.started_at,
        finished_at: wf.finished_at,
        prompt_ids: wf.prompt_ids.clone(),
        blocked_by,
    }
}

fn build_apply_detail(
    node: &dag::DagNode,
    idx: usize,
    dag: &Dag,
    runtime: &WorkflowRuntime,
) -> DetailNode {
    let dispatch = runtime.apply.get(&idx);
    let state = node_state_label(dispatch);
    let depends_on: Vec<String> = node
        .deps
        .iter()
        .filter_map(|d| dag.nodes.get(*d).map(|n| n.id.clone()))
        .collect();
    DetailNode {
        id: node.id.clone(),
        label: node.label.clone(),
        state,
        prompt_id: dispatch.and_then(|d| d.prompt_id),
        prompt_uuid: dispatch.and_then(|d| d.uuid.clone()),
        exit_code: dispatch.and_then(|d| d.exit_code),
        depends_on,
    }
}

fn build_phase_detail(id: &str, label: &str, dispatch: &NodeDispatch) -> DetailNode {
    DetailNode {
        id: id.to_string(),
        label: label.to_string(),
        state: node_state_label(Some(dispatch)),
        prompt_id: dispatch.prompt_id,
        prompt_uuid: dispatch.uuid.clone(),
        exit_code: dispatch.exit_code,
        depends_on: Vec::new(),
    }
}

/// Map a [`NodeDispatch`] to the wire-format lifecycle label. Mirrors
/// the orchestrator's own state machine: a node we never dispatched is
/// `pending`; a dispatched node with no `WorkerFinished` is `running`;
/// a finished worker that ticked the boxes is `completed`; everything
/// else (non-zero exit code, or finished without ticking) is `failed`.
fn node_state_label(d: Option<&NodeDispatch>) -> String {
    match d {
        None => "pending".into(),
        Some(d) if !d.finished => "running".into(),
        Some(d) if d.completed && d.exit_code.unwrap_or(0) == 0 => "completed".into(),
        Some(_) => "failed".into(),
    }
}

fn all_apply_complete(runtime: &WorkflowRuntime, dag: &Dag) -> bool {
    if dag.nodes.is_empty() {
        return false;
    }
    (0..dag.nodes.len()).all(|i| {
        runtime
            .apply
            .get(&i)
            .is_some_and(|d| d.completed)
    })
}

#[derive(Debug, PartialEq)]
enum TagTarget {
    Apply { node_id: String },
    Verify,
    Archive,
}

fn expected_apply_tag(workflow: &str, node_id: &str) -> String {
    format!("{TAG_PREFIX}/wf={workflow}/phase=apply/node={node_id}")
}

/// Parse `clhorde-scheduler/wf=<name>/phase=<phase>[/node=<id>]` out of a
/// prompt's tags. Returns the workflow name and the parsed target.
fn parse_scheduler_tag(tags: &[String]) -> Option<(String, TagTarget)> {
    for tag in tags {
        let Some(rest) = tag.strip_prefix(&format!("{TAG_PREFIX}/")) else {
            continue;
        };
        let mut name: Option<String> = None;
        let mut phase: Option<String> = None;
        let mut node_id: Option<String> = None;
        for part in rest.split('/') {
            if let Some(v) = part.strip_prefix("wf=") {
                name = Some(v.to_string());
            } else if let Some(v) = part.strip_prefix("phase=") {
                phase = Some(v.to_string());
            } else if let Some(v) = part.strip_prefix("node=") {
                node_id = Some(v.to_string());
            }
        }
        let (n, p) = match (name, phase) {
            (Some(n), Some(p)) => (n, p),
            _ => continue,
        };
        let target = match p.as_str() {
            "apply" => TagTarget::Apply {
                node_id: node_id.unwrap_or_default(),
            },
            "verify" => TagTarget::Verify,
            "archive" => TagTarget::Archive,
            _ => continue,
        };
        return Some((n, target));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use clhorde_core::protocol::PromptInfo;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> (
        TempDir,
        Orchestrator,
        mpsc::UnboundedReceiver<ClientRequest>,
    ) {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("store"));
        let (tx, rx) = mpsc::unbounded_channel();
        let orch = Orchestrator::new(tmp.path(), store, tx);
        (tmp, orch, rx)
    }

    fn change_dir(tmp: &TempDir, name: &str) -> PathBuf {
        let p = tmp.path().join("openspec").join("changes").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_marker(tmp: &TempDir, name: &str, body: &str) {
        fs::write(change_dir(tmp, name).join(".clhorde-ready"), body).unwrap();
    }

    fn remove_marker(tmp: &TempDir, name: &str) {
        let _ = fs::remove_file(change_dir(tmp, name).join(".clhorde-ready"));
    }

    fn write_tasks(tmp: &TempDir, name: &str, body: &str) {
        fs::write(change_dir(tmp, name).join("tasks.md"), body).unwrap();
    }

    fn drain_requests(
        rx: &mut mpsc::UnboundedReceiver<ClientRequest>,
    ) -> Vec<ClientRequest> {
        let mut out = Vec::new();
        while let Ok(r) = rx.try_recv() {
            out.push(r);
        }
        out
    }

    fn submit_text(req: &ClientRequest) -> &str {
        match req {
            ClientRequest::SubmitPrompt { text, .. } => text,
            _ => panic!("expected SubmitPrompt, got {req:?}"),
        }
    }

    fn submit_tag(req: &ClientRequest) -> &str {
        match req {
            ClientRequest::SubmitPrompt { tags, .. } => tags.first().map(|s| s.as_str()).unwrap_or(""),
            _ => panic!("expected SubmitPrompt"),
        }
    }

    fn fake_prompt_added(id: usize, tag: &str) -> DaemonEvent {
        DaemonEvent::PromptAdded(PromptInfo {
            id,
            text: String::new(),
            cwd: None,
            mode: "oneshot".into(),
            status: "Pending".into(),
            output: None,
            error: None,
            worktree: true,
            worktree_path: None,
            session_id: None,
            tags: vec![tag.into()],
            queue_rank: 0.0,
            seen: false,
            resume: false,
            output_len: 0,
            elapsed_secs: None,
            uuid: format!("uuid-{id}"),
            has_pty: false,
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
            worktree_id: Some("".into()),
            annotations: BTreeMap::new(),
        })
    }

    // ── apply-phase dispatch ──

    #[test]
    fn marker_with_tasks_dispatches_first_apply_node() {
        let (tmp, mut orch, mut rx) = fixture();
        write_marker(&tmp, "x", "");
        write_tasks(
            &tmp,
            "x",
            "## 1. A\n- [ ] 1.1 a\n## 2. B\n- [ ] 2.1 b\n",
        );
        // tasks.md first so it's parsed before the marker triggers advance.
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let reqs = drain_requests(&mut rx);
        assert_eq!(reqs.len(), 1);
        let tag = submit_tag(&reqs[0]);
        assert!(tag.starts_with("clhorde-scheduler/wf=x/phase=apply/node=1"), "tag = {tag}");
        let text = submit_text(&reqs[0]);
        assert!(text.contains("section 1"));
        assert!(text.contains("- [ ] 1.1 a"));
        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Implementing
        );
    }

    #[test]
    fn submit_uses_workflow_name_as_worktree_id() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let req = drain_requests(&mut rx).pop().unwrap();
        match req {
            ClientRequest::SubmitPrompt {
                worktree,
                worktree_id,
                ..
            } => {
                assert!(worktree);
                assert_eq!(worktree_id.as_deref(), Some("x"));
            }
            _ => panic!("expected SubmitPrompt"),
        }
    }

    #[test]
    fn second_node_dispatches_after_first_completes() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n## 2. B\n- [ ] 2.1 b\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let first = drain_requests(&mut rx).pop().unwrap();
        let first_tag = submit_tag(&first).to_string();
        // Daemon "echoes" the prompt with id 100 and our tag.
        orch.handle_daemon_event(&fake_prompt_added(100, &first_tag))
            .unwrap();

        // User/Claude ticks the box.
        write_tasks(&tmp, "x", "## 1. A\n- [x] 1.1 a\n## 2. B\n- [ ] 2.1 b\n");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();

        // Worker finishes successfully.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 100,
            exit_code: Some(0),
        })
        .unwrap();

        let new_reqs = drain_requests(&mut rx);
        assert_eq!(new_reqs.len(), 1);
        let tag = submit_tag(&new_reqs[0]);
        assert!(tag.contains("/node=2"), "tag={tag}");

        // Predecessor's daemon id propagated as depends_on.
        match &new_reqs[0] {
            ClientRequest::SubmitPrompt { depends_on, .. } => {
                assert_eq!(depends_on, &vec![100]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn worker_exit_zero_with_unchecked_tasks_fails_workflow() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let req = drain_requests(&mut rx).pop().unwrap();
        let tag = submit_tag(&req).to_string();
        orch.handle_daemon_event(&fake_prompt_added(7, &tag))
            .unwrap();

        // Worker exits cleanly but tasks.md is still unchecked.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 7,
            exit_code: Some(0),
        })
        .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert!(matches!(wf.status, WorkflowStatus::Failed { .. }));
        if let WorkflowStatus::Failed { reason } = &wf.status {
            assert!(reason.contains("tasks remain unchecked"));
        }
    }

    #[test]
    fn worker_nonzero_exit_fails_workflow() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let req = drain_requests(&mut rx).pop().unwrap();
        let tag = submit_tag(&req).to_string();
        orch.handle_daemon_event(&fake_prompt_added(9, &tag))
            .unwrap();

        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 9,
            exit_code: Some(2),
        })
        .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert!(matches!(wf.status, WorkflowStatus::Failed { .. }));
    }

    // ── verify / archive lifecycle ──

    #[test]
    fn finishing_apply_dispatches_verify_then_archive_then_archives() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        // Apply → ack → done.
        let req = drain_requests(&mut rx).pop().unwrap();
        let tag = submit_tag(&req).to_string();
        orch.handle_daemon_event(&fake_prompt_added(1, &tag)).unwrap();
        write_tasks(&tmp, "x", "## 1. A\n- [x] 1.1 a\n");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 1,
            exit_code: Some(0),
        })
        .unwrap();

        // Verify dispatched.
        let reqs = drain_requests(&mut rx);
        assert_eq!(reqs.len(), 1);
        let verify_tag = submit_tag(&reqs[0]).to_string();
        assert!(verify_tag.contains("phase=verify"));
        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Verifying
        );

        // Verify completes.
        orch.handle_daemon_event(&fake_prompt_added(2, &verify_tag))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 2,
            exit_code: Some(0),
        })
        .unwrap();

        let reqs = drain_requests(&mut rx);
        assert_eq!(reqs.len(), 1);
        let archive_tag = submit_tag(&reqs[0]).to_string();
        assert!(archive_tag.contains("phase=archive"));
        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Archiving
        );

        // Archive completes.
        orch.handle_daemon_event(&fake_prompt_added(3, &archive_tag))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 3,
            exit_code: Some(0),
        })
        .unwrap();

        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Archived
        );
    }

    #[test]
    fn verify_failure_marks_workflow_failed() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        let apply_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(1, &apply_tag))
            .unwrap();
        write_tasks(&tmp, "x", "## 1. A\n- [x] 1.1 a\n");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 1,
            exit_code: Some(0),
        })
        .unwrap();

        let verify_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(2, &verify_tag))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 2,
            exit_code: Some(1),
        })
        .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert!(matches!(wf.status, WorkflowStatus::Failed { .. }));
    }

    // ── tag parsing ──

    #[test]
    fn parses_apply_tag() {
        let (n, t) = parse_scheduler_tag(&[
            "clhorde-scheduler/wf=add-oauth/phase=apply/node=1.2".into(),
        ])
        .unwrap();
        assert_eq!(n, "add-oauth");
        assert_eq!(
            t,
            TagTarget::Apply {
                node_id: "1.2".into()
            }
        );
    }

    #[test]
    fn parses_verify_tag() {
        let (n, t) = parse_scheduler_tag(&[
            "clhorde-scheduler/wf=add-oauth/phase=verify".into(),
        ])
        .unwrap();
        assert_eq!(n, "add-oauth");
        assert_eq!(t, TagTarget::Verify);
    }

    #[test]
    fn parses_archive_tag() {
        let (n, t) = parse_scheduler_tag(&[
            "clhorde-scheduler/wf=add-oauth/phase=archive".into(),
        ])
        .unwrap();
        assert_eq!(n, "add-oauth");
        assert_eq!(t, TagTarget::Archive);
    }

    #[test]
    fn unrelated_tags_are_ignored() {
        assert!(parse_scheduler_tag(&["user-tag".into()]).is_none());
        assert!(parse_scheduler_tag(&["clhorde-scheduler/garbage".into()]).is_none());
    }

    // ── openspec.affected_changes (Phase 2.5) ──

    fn make_prompt_info(
        id: usize,
        cwd: Option<&Path>,
        worktree_path: Option<&Path>,
    ) -> PromptInfo {
        PromptInfo {
            id,
            text: String::new(),
            cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
            mode: "oneshot".into(),
            status: "Pending".into(),
            output: None,
            error: None,
            worktree: worktree_path.is_some(),
            worktree_path: worktree_path.map(|p| p.to_string_lossy().into_owned()),
            session_id: None,
            tags: Vec::new(),
            queue_rank: 0.0,
            seen: false,
            resume: false,
            output_len: 0,
            elapsed_secs: None,
            uuid: format!("uuid-{id}"),
            has_pty: false,
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
            worktree_id: None,
            annotations: BTreeMap::new(),
        }
    }

    fn write_change_file(root: &Path, change: &str, file: &str, body: &str) {
        let p = root.join("openspec").join("changes").join(change);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join(file), body).unwrap();
    }

    fn find_set_annotation(
        rx: &mut mpsc::UnboundedReceiver<ClientRequest>,
        prompt_id: usize,
    ) -> Option<(String, serde_json::Value)> {
        for req in drain_requests(rx) {
            if let ClientRequest::SetAnnotation {
                prompt_id: pid,
                key,
                value,
            } = req
            {
                if pid == prompt_id {
                    return Some((key, value));
                }
            }
        }
        None
    }

    #[test]
    fn worker_lifecycle_emits_affected_changes_annotation() {
        let (tmp, mut orch, mut rx) = fixture();
        // Pre-existing change directory.
        write_change_file(tmp.path(), "add-oauth", "proposal.md", "v1");

        // Daemon tells us about a prompt that runs in tmp.
        let info = make_prompt_info(42, Some(tmp.path()), None);
        orch.handle_daemon_event(&DaemonEvent::PromptAdded(info))
            .unwrap();
        // Drain anything from PromptAdded itself (none expected).
        drain_requests(&mut rx);

        // WorkerStarted: baseline snapshot taken.
        orch.handle_daemon_event(&DaemonEvent::WorkerStarted { prompt_id: 42 })
            .unwrap();

        // Worker writes to one change directory.
        write_change_file(tmp.path(), "add-oauth", "proposal.md", "v2 different");
        // And touches a brand-new change.
        write_change_file(tmp.path(), "fix-login", "proposal.md", "new");

        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 42,
            exit_code: Some(0),
        })
        .unwrap();

        let (key, value) = find_set_annotation(&mut rx, 42).expect("annotation");
        assert_eq!(key, "openspec.affected_changes");
        let arr = value.as_array().unwrap();
        let names: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(names, vec!["add-oauth", "fix-login"]);
    }

    #[test]
    fn worker_lifecycle_emits_empty_list_when_nothing_changed() {
        // Plan note: an empty list is informative — distinguishes
        // "watched, nothing changed" from "scheduler missed this prompt".
        let (tmp, mut orch, mut rx) = fixture();
        write_change_file(tmp.path(), "add-oauth", "proposal.md", "v1");

        let info = make_prompt_info(7, Some(tmp.path()), None);
        orch.handle_daemon_event(&DaemonEvent::PromptAdded(info))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerStarted { prompt_id: 7 })
            .unwrap();
        // No FS changes between started and finished.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 7,
            exit_code: Some(0),
        })
        .unwrap();

        let (_key, value) = find_set_annotation(&mut rx, 7).expect("annotation");
        assert_eq!(value, serde_json::json!([]));
    }

    #[test]
    fn worker_finished_without_baseline_emits_no_annotation() {
        // The scheduler joined mid-flight and missed WorkerStarted —
        // no baseline, no annotation.
        let (tmp, mut orch, mut rx) = fixture();
        write_change_file(tmp.path(), "add-oauth", "proposal.md", "v1");

        let info = make_prompt_info(99, Some(tmp.path()), None);
        orch.handle_daemon_event(&DaemonEvent::PromptAdded(info))
            .unwrap();
        // Skip WorkerStarted entirely.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 99,
            exit_code: Some(0),
        })
        .unwrap();

        assert!(find_set_annotation(&mut rx, 99).is_none());
    }

    #[test]
    fn worker_started_without_known_cwd_does_not_panic() {
        // Daemon emitted WorkerStarted for a prompt we never saw a
        // PromptAdded for — silently no-op.
        let (_tmp, mut orch, _rx) = fixture();
        orch.handle_daemon_event(&DaemonEvent::WorkerStarted { prompt_id: 1234 })
            .unwrap();
        // (no assertion — the only contract is "no panic, no annotation").
    }

    #[test]
    fn worktree_path_takes_precedence_over_cwd_for_snapshot() {
        // The scheduler should snapshot the worktree (where edits actually
        // land), not the original cwd.
        let (tmp, mut orch, mut rx) = fixture();
        let cwd = TempDir::new().unwrap();
        let worktree = TempDir::new().unwrap();

        write_change_file(cwd.path(), "in-cwd-only", "f.md", "x");
        write_change_file(worktree.path(), "in-worktree", "f.md", "x");

        let info = make_prompt_info(5, Some(cwd.path()), Some(worktree.path()));
        orch.handle_daemon_event(&DaemonEvent::PromptAdded(info))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerStarted { prompt_id: 5 })
            .unwrap();

        // Edit only the worktree.
        write_change_file(worktree.path(), "in-worktree", "f.md", "y");
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 5,
            exit_code: Some(0),
        })
        .unwrap();

        let (_key, value) = find_set_annotation(&mut rx, 5).expect("annotation");
        let names: Vec<&str> = value
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["in-worktree"]);
        // Sanity: keep tmp/cwd alive until end of test.
        let _ = (tmp, cwd, worktree);
    }

    #[test]
    fn prompt_removed_drops_baseline_and_cwd() {
        let (tmp, mut orch, _rx) = fixture();
        write_change_file(tmp.path(), "x", "f.md", "x");

        let info = make_prompt_info(1, Some(tmp.path()), None);
        orch.handle_daemon_event(&DaemonEvent::PromptAdded(info))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerStarted { prompt_id: 1 })
            .unwrap();
        assert!(orch.prompt_baselines.contains_key(&1));
        assert!(orch.prompt_cwds.contains_key(&1));

        orch.handle_daemon_event(&DaemonEvent::PromptRemoved { prompt_id: 1 })
            .unwrap();
        assert!(!orch.prompt_baselines.contains_key(&1));
        assert!(!orch.prompt_cwds.contains_key(&1));
    }

    #[test]
    fn state_snapshot_populates_cwd_map_for_running_prompts() {
        // After reconnect, the daemon ships a full StateSnapshot. We use
        // it to populate cwds so future WorkerFinished events can still
        // produce diffs (when paired with a future WorkerStarted).
        use clhorde_core::protocol::{DaemonState, PROTOCOL_VERSION};

        let (tmp, mut orch, _rx) = fixture();
        let info = make_prompt_info(11, Some(tmp.path()), None);
        let state = DaemonState {
            prompts: vec![info],
            max_workers: 1,
            active_workers: 0,
            default_mode: "interactive".into(),
            protocol_version: PROTOCOL_VERSION,
        };
        orch.handle_daemon_event(&DaemonEvent::StateSnapshot(state))
            .unwrap();
        assert_eq!(
            orch.prompt_cwds.get(&11).map(PathBuf::as_path),
            Some(tmp.path())
        );
    }

    // ── pre-existing FS-only tests preserved ──

    #[test]
    fn marker_remove_for_unknown_workflow_is_silently_ignored() {
        let (_tmp, mut orch, _rx) = fixture();
        orch.handle_event(FsEvent::MarkerRemoved {
            name: "ghost".into(),
        })
        .unwrap();
        assert!(orch.workflow("ghost").is_none());
    }

    #[test]
    fn marker_create_then_re_create_refreshes_metadata_only() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "x", "priority = 1\n");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        let queued_at_first = orch.workflow("x").unwrap().queued_at;

        write_marker(&tmp, "x", "priority = 9\n");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata.priority, Some(9));
        assert_eq!(wf.queued_at, queued_at_first);
    }

    #[test]
    fn tasks_modified_caches_parsed_sections() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        write_tasks(
            &tmp,
            "x",
            "## 1. A\n- [ ] 1.1 first\n## 2. B\n- [x] 2.1 done\n",
        );
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();

        let sections = orch.tasks_for("x");
        assert_eq!(sections.len(), 2);
    }

    #[test]
    fn missing_tasks_md_clears_cache_silently() {
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        assert!(orch.tasks_for("x").is_empty());
    }

    #[test]
    fn malformed_marker_falls_back_to_defaults() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "x", "this isn't [valid TOML");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        let wf = orch.workflow("x").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata, MarkerMetadata::default());
    }

    #[test]
    fn marker_removed_unqueues_a_queued_workflow() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        remove_marker(&tmp, "x");
        orch.handle_event(FsEvent::MarkerRemoved { name: "x".into() })
            .unwrap();
        assert_eq!(orch.workflow("x").unwrap().status, WorkflowStatus::Drafted);
    }

    #[test]
    fn marker_removed_cancels_running_workflow() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        // Drain the dispatched apply prompt; workflow is now Implementing.
        let _ = drain_requests(&mut rx);

        remove_marker(&tmp, "x");
        orch.handle_event(FsEvent::MarkerRemoved { name: "x".into() })
            .unwrap();
        assert_eq!(orch.workflow("x").unwrap().status, WorkflowStatus::Cancelled);
    }

    // ── reconcile / restart ──

    #[test]
    fn reconcile_picks_up_existing_marker() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "add-oauth", "priority = 3\n");
        orch.reconcile().unwrap();
        let wf = orch.workflow("add-oauth").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata.priority, Some(3));
    }

    #[test]
    fn reconcile_unqueues_when_marker_disappeared() {
        let (tmp, mut orch1, _rx) = fixture();
        write_marker(&tmp, "x", "");
        orch1.reconcile().unwrap();

        remove_marker(&tmp, "x");
        let store = orch1.store.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut orch2 = Orchestrator::new(tmp.path(), store, tx);
        orch2.reconcile().unwrap();

        assert_eq!(orch2.workflow("x").unwrap().status, WorkflowStatus::Drafted);
    }

    #[test]
    fn reconcile_keeps_drafts_drafted() {
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "draft-only");
        orch.reconcile().unwrap();
        assert_eq!(
            orch.workflow("draft-only").unwrap().status,
            WorkflowStatus::Drafted
        );
    }

    #[test]
    fn reconcile_caches_tasks_md() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "x", "");
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 q\n");
        orch.reconcile().unwrap();
        assert_eq!(orch.tasks_for("x").len(), 1);
    }

    #[test]
    fn reconcile_is_idempotent() {
        let (tmp, mut orch, _rx) = fixture();
        write_marker(&tmp, "x", "priority = 1\n");
        orch.reconcile().unwrap();
        let wf_before = orch.workflow("x").unwrap().clone();
        orch.reconcile().unwrap();
        let wf_after = orch.workflow("x").unwrap();
        assert_eq!(wf_after.queued_at, wf_before.queued_at);
        assert_eq!(wf_after.metadata, wf_before.metadata);
    }

    #[test]
    fn reconcile_refreshes_metadata_when_marker_was_edited_offline() {
        let (tmp, mut orch1, _rx) = fixture();
        write_marker(&tmp, "x", "priority = 1\n");
        orch1.reconcile().unwrap();

        write_marker(&tmp, "x", "priority = 99\n");
        let store = orch1.store.clone();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut orch2 = Orchestrator::new(tmp.path(), store, tx);
        orch2.reconcile().unwrap();

        assert_eq!(
            orch2.workflow("x").unwrap().metadata.priority,
            Some(99)
        );
    }

    // ── queue_workflow ──

    #[test]
    fn queue_workflow_writes_marker_and_transitions() {
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        orch.queue_workflow("x", Some(3)).unwrap();
        // Marker landed on disk with the expected priority body.
        let body = fs::read_to_string(
            tmp.path().join("openspec/changes/x/.clhorde-ready"),
        )
        .unwrap();
        assert_eq!(body, "priority = 3\n");
        // Workflow is now Queued in memory.
        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Queued
        );
        assert_eq!(orch.workflow("x").unwrap().metadata.priority, Some(3));
    }

    #[test]
    fn queue_workflow_without_priority_writes_empty_marker() {
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        orch.queue_workflow("x", None).unwrap();
        let body = fs::read_to_string(
            tmp.path().join("openspec/changes/x/.clhorde-ready"),
        )
        .unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn queue_workflow_rejects_missing_change_dir() {
        let (_tmp, mut orch, _rx) = fixture();
        let err = orch.queue_workflow("ghost", None).unwrap_err();
        assert!(matches!(err, OrchestratorError::NotFound(_)));
    }

    #[test]
    fn detail_for_drafted_workflow_has_empty_apply() {
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        orch.reconcile().unwrap();
        let d = orch.detail("x").unwrap();
        assert_eq!(d.name, "x");
        assert_eq!(d.status, "drafted");
        assert!(d.apply.is_empty());
    }

    #[test]
    fn detail_for_in_flight_workflow_lists_dag_nodes() {
        // After a marker + tasks.md land, the orchestrator parses the DAG
        // and dispatches the first apply node. The detail should expose
        // both nodes — node 1 dispatched (running, has prompt info) and
        // node 2 still pending (no dispatch yet because of the
        // sequential default).
        let (tmp, mut orch, mut rx) = fixture();
        write_marker(&tmp, "x", "");
        write_tasks(
            &tmp,
            "x",
            "## 1. Theme\n- [ ] 1.1 a\n## 2. UI\n- [ ] 2.1 b\n",
        );
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        // Pick up the dispatched prompt's tag and feed PromptAdded back.
        let req = drain_requests(&mut rx).into_iter().next().unwrap();
        let tag = match req {
            ClientRequest::SubmitPrompt { tags, .. } => tags[0].clone(),
            _ => panic!("expected SubmitPrompt"),
        };
        orch.handle_daemon_event(&fake_prompt_added(42, &tag)).unwrap();

        let d = orch.detail("x").unwrap();
        assert_eq!(d.apply.len(), 2);
        assert_eq!(d.apply[0].id, "1");
        assert_eq!(d.apply[0].label, "Theme");
        assert_eq!(d.apply[0].state, "running");
        assert_eq!(d.apply[0].prompt_id, Some(42));
        assert!(d.apply[0].depends_on.is_empty());
        // Node 2 is sequential after node 1 — pending and depends on "1".
        assert_eq!(d.apply[1].id, "2");
        assert_eq!(d.apply[1].state, "pending");
        assert!(d.apply[1].prompt_id.is_none());
        assert_eq!(d.apply[1].depends_on, vec!["1".to_string()]);
    }

    #[test]
    fn detail_unknown_workflow_returns_none() {
        let (_tmp, orch, _rx) = fixture();
        assert!(orch.detail("ghost").is_none());
    }

    #[test]
    fn queue_workflow_then_cancel_unqueues() {
        // Round-trip: Q action → X action puts the workflow back to
        // Drafted and removes the marker.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        orch.queue_workflow("x", None).unwrap();
        let kind = orch.cancel_workflow("x").unwrap();
        assert_eq!(kind, "unqueued");
        assert!(!tmp
            .path()
            .join("openspec/changes/x/.clhorde-ready")
            .exists());
        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Drafted
        );
    }

    // ── Phase 5.1: SchedulerEvent broadcast emission ──
    //
    // The orchestrator emits a [`SchedulerEvent::WorkflowUpdated`] for
    // every workflow whose [`WorkflowSummary`] changed across a public
    // method call. Idempotent calls (no state change) emit nothing —
    // that's important for keeping a Subscribe stream quiet when the
    // user isn't doing anything.

    fn drain_events(
        rx: &mut broadcast::Receiver<SchedulerEvent>,
    ) -> Vec<SchedulerEvent> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // The capacity is large enough that test workloads
                    // never lag in practice. If they ever do, we want
                    // the test to fail loudly rather than silently
                    // drop events.
                    panic!("event broadcast lagged in test")
                }
            }
        }
        out
    }

    fn updated_summary(ev: &SchedulerEvent) -> &WorkflowSummary {
        match ev {
            SchedulerEvent::WorkflowUpdated { summary } => summary,
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }
    }

    #[test]
    fn queue_workflow_emits_workflow_updated_event() {
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut events = orch.events_subscribe();

        orch.queue_workflow("x", Some(5)).unwrap();

        let evs = drain_events(&mut events);
        assert_eq!(evs.len(), 1, "expected exactly one event, got {evs:?}");
        let s = updated_summary(&evs[0]);
        assert_eq!(s.name, "x");
        assert_eq!(s.status, "queued");
        assert_eq!(s.priority, 5);
    }

    #[test]
    fn cancel_workflow_emits_workflow_updated_event() {
        // Setup: queue → confirm one event → drain → cancel → expect a
        // second event reflecting the Drafted transition.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut events = orch.events_subscribe();
        orch.queue_workflow("x", None).unwrap();
        drain_events(&mut events); // discard the queue event

        orch.cancel_workflow("x").unwrap();

        let evs = drain_events(&mut events);
        assert_eq!(evs.len(), 1);
        let s = updated_summary(&evs[0]);
        assert_eq!(s.name, "x");
        assert_eq!(s.status, "drafted");
    }

    #[test]
    fn idempotent_calls_emit_no_events() {
        // try_advance on a Drafted workflow does nothing — no event
        // should fire. Ditto cancel on an already-Drafted workflow.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut events = orch.events_subscribe();
        orch.reconcile().unwrap();
        // Discard the reconcile event that turned the FS-discovered
        // change into a Drafted workflow.
        drain_events(&mut events);

        orch.try_advance("x").unwrap();
        assert!(drain_events(&mut events).is_empty());

        // cancel on Drafted reports "noop" and emits no event.
        let kind = orch.cancel_workflow("x").unwrap();
        assert_eq!(kind, "noop");
        assert!(drain_events(&mut events).is_empty());
    }

    #[test]
    fn reconcile_emits_one_event_per_discovered_change() {
        // Two changes on disk → reconcile inserts both → exactly two
        // WorkflowUpdated events surface.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        change_dir(&tmp, "y");
        let mut events = orch.events_subscribe();

        orch.reconcile().unwrap();

        let evs = drain_events(&mut events);
        assert_eq!(evs.len(), 2);
        let mut names: Vec<&str> = evs.iter().map(|e| updated_summary(e).name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["x", "y"]);
    }

    #[test]
    fn handle_event_emits_on_marker_creation() {
        // FS event → MarkerCreated → state transitions Drafted→Queued
        // → one WorkflowUpdated.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        write_marker(&tmp, "x", "");
        // First reconcile inserts the workflow as Queued (since the
        // marker is already on disk). We want to drive a state change
        // *after* subscription, so set up the subscribe AFTER the
        // initial reconcile and then simulate marker re-creation.
        orch.reconcile().unwrap();

        let mut events = orch.events_subscribe();
        // Removing then re-creating the marker round-trips through
        // Drafted and back to Queued — two events.
        remove_marker(&tmp, "x");
        orch
            .handle_event(FsEvent::MarkerRemoved { name: "x".into() })
            .unwrap();
        write_marker(&tmp, "x", "priority = 1\n");
        orch
            .handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let evs = drain_events(&mut events);
        assert_eq!(evs.len(), 2);
        assert_eq!(updated_summary(&evs[0]).status, "drafted");
        assert_eq!(updated_summary(&evs[1]).status, "queued");
        assert_eq!(updated_summary(&evs[1]).priority, 1);
    }

    #[test]
    fn events_subscribe_can_have_multiple_subscribers() {
        // Each Subscribe connection grabs its own broadcast::Receiver.
        // The same event must reach every subscriber (i.e. the
        // broadcast really is fan-out, not pick-one).
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut a = orch.events_subscribe();
        let mut b = orch.events_subscribe();

        orch.queue_workflow("x", None).unwrap();

        let from_a = drain_events(&mut a);
        let from_b = drain_events(&mut b);
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_a[0], from_b[0]);
    }

    // ── Phase 5.3: detail-event broadcast emission ──
    //
    // The dedicated `detail_events` channel emits one
    // `SchedulerEvent::DetailUpdated` per workflow whose
    // `WorkflowDetail` differs across a public mutating call.

    fn updated_detail(ev: &SchedulerEvent) -> &WorkflowDetail {
        match ev {
            SchedulerEvent::DetailUpdated { detail } => detail,
            other => panic!("expected DetailUpdated, got {other:?}"),
        }
    }

    #[test]
    fn queue_workflow_emits_detail_updated_event() {
        // Same shape as the summary diff — queueing a fresh draft
        // shifts both the summary and the detail (status flips
        // drafted→queued; the detail mirrors that on its `status`
        // field). Both channels emit exactly once.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut summary_rx = orch.events_subscribe();
        let mut detail_rx = orch.detail_events_subscribe();

        orch.queue_workflow("x", Some(2)).unwrap();

        let summaries = drain_events(&mut summary_rx);
        assert_eq!(summaries.len(), 1);
        let details = drain_events(&mut detail_rx);
        assert_eq!(details.len(), 1, "expected exactly one detail event, got {details:?}");
        let d = updated_detail(&details[0]);
        assert_eq!(d.name, "x");
        assert_eq!(d.status, "queued");
        assert_eq!(d.priority, 2);
    }

    #[test]
    fn idempotent_calls_emit_no_detail_events() {
        // Mirrors `idempotent_calls_emit_no_events` for the detail
        // channel: try_advance on a Drafted workflow is a no-op, no
        // event on either channel.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut detail_rx = orch.detail_events_subscribe();
        orch.reconcile().unwrap();
        drain_events(&mut detail_rx); // discard the reconcile event

        orch.try_advance("x").unwrap();
        assert!(drain_events(&mut detail_rx).is_empty());
    }

    #[test]
    fn detail_subscribers_independent_from_summary_subscribers() {
        // A subscriber that only takes detail_events_subscribe doesn't
        // get summary events, and vice versa. Important for the
        // SubscribeDetail wire: per-workflow viewers should never see
        // unrelated WorkflowUpdated frames.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut detail_rx = orch.detail_events_subscribe();
        let mut summary_rx = orch.events_subscribe();

        orch.queue_workflow("x", None).unwrap();

        let summaries = drain_events(&mut summary_rx);
        let details = drain_events(&mut detail_rx);
        assert_eq!(summaries.len(), 1);
        assert_eq!(details.len(), 1);
        assert!(matches!(summaries[0], SchedulerEvent::WorkflowUpdated { .. }));
        assert!(matches!(details[0], SchedulerEvent::DetailUpdated { .. }));
    }

    #[test]
    fn detail_events_fan_out_to_multiple_subscribers() {
        // Two SubscribeDetail connections on the same workflow each
        // receive the broadcast (after server-side filter, but the
        // broadcast itself fans out unfiltered).
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut a = orch.detail_events_subscribe();
        let mut b = orch.detail_events_subscribe();

        orch.queue_workflow("x", None).unwrap();

        let from_a = drain_events(&mut a);
        let from_b = drain_events(&mut b);
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_b.len(), 1);
        assert_eq!(from_a[0], from_b[0]);
    }

    #[test]
    fn detail_events_carry_per_workflow_payload() {
        // Two workflows, two queues — each workflow's DetailUpdated
        // carries its own name. Lets the control-server filter prune
        // by name reliably.
        let (tmp, mut orch, _rx) = fixture();
        change_dir(&tmp, "x");
        change_dir(&tmp, "y");
        let mut detail_rx = orch.detail_events_subscribe();

        orch.queue_workflow("x", None).unwrap();
        orch.queue_workflow("y", None).unwrap();

        let evs = drain_events(&mut detail_rx);
        assert_eq!(evs.len(), 2);
        let mut names: Vec<&str> = evs.iter().map(|e| updated_detail(e).name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["x", "y"]);
    }

    // ── inter-workflow dependency gate (Phase 5.4.1) ──

    /// Build a fully-archived `Workflow` ready to be saved into a store
    /// fixture. Used as the "satisfied dep" in the gate tests below.
    fn archived_workflow(name: &str) -> Workflow {
        let mut w = Workflow::drafted(name);
        w.queue(MarkerMetadata::default()).unwrap();
        w.start_implementing().unwrap();
        w.start_verifying().unwrap();
        w.start_archiving().unwrap();
        w.archive().unwrap();
        w
    }

    fn assert_failed(orch: &Orchestrator, name: &str, reason_contains: &str) {
        match &orch.workflow(name).unwrap().status {
            WorkflowStatus::Failed { reason } => {
                assert!(
                    reason.contains(reason_contains),
                    "expected reason to contain {reason_contains:?}, got: {reason}"
                );
            }
            other => panic!("expected {name} to be Failed, got {other:?}"),
        }
    }

    #[test]
    fn queued_with_archived_dep_proceeds_to_implementing() {
        let (tmp, mut orch, mut rx) = fixture();
        // Pre-populate the store with `base` already archived. Reconcile
        // loads it back into the in-memory map so `evaluate` sees it.
        orch.store.save(&archived_workflow("base")).unwrap();

        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");
        orch.reconcile().unwrap();

        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Implementing
        );
        let reqs = drain_requests(&mut rx);
        assert_eq!(reqs.len(), 1, "expected one apply prompt for x");
        assert!(submit_tag(&reqs[0]).contains("wf=x/phase=apply"));
    }

    #[test]
    fn queued_with_pending_dep_stays_queued_and_dispatches_nothing() {
        let (tmp, mut orch, mut rx) = fixture();
        // `base` is on disk but never queued — it stays Drafted, which
        // counts as "not Archived" → the dependent must hold.
        change_dir(&tmp, "base");
        write_tasks(&tmp, "base", "## 1. A\n- [ ] 1.1 a\n");

        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");
        orch.reconcile().unwrap();

        assert_eq!(orch.workflow("x").unwrap().status, WorkflowStatus::Queued);
        assert!(
            drain_requests(&mut rx).is_empty(),
            "no prompt should have been dispatched while x is blocked"
        );
    }

    #[test]
    fn queued_with_missing_dep_fails() {
        let (tmp, mut orch, _rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"ghost\"]\n");

        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        assert_failed(&orch, "x", "ghost");
    }

    #[test]
    fn queued_with_cancelled_dep_fails() {
        let (tmp, mut orch, _rx) = fixture();
        // Save a Cancelled `base` directly. Reconcile loads it back.
        let mut base = Workflow::drafted("base");
        base.queue(MarkerMetadata::default()).unwrap();
        base.cancel().unwrap();
        orch.store.save(&base).unwrap();

        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");
        orch.reconcile().unwrap();

        assert_failed(&orch, "x", "cancelled");
    }

    #[test]
    fn cycle_between_two_queued_workflows_fails_them() {
        let (tmp, mut orch, _rx) = fixture();
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_tasks(&tmp, "y", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"y\"]\n");
        write_marker(&tmp, "y", "depends_on = [\"x\"]\n");

        orch.reconcile().unwrap();

        // Whichever side reconcile evaluates first detects the cycle and
        // fails. The other one then sees the first is Failed and fails
        // too via the `failed_dep_propagates_reason` arm. Either way both
        // end up Failed; the *first* may carry the cycle reason while the
        // *second* may carry the propagated-failure reason.
        let x_status = orch.workflow("x").unwrap().status.clone();
        let y_status = orch.workflow("y").unwrap().status.clone();
        assert!(
            matches!(x_status, WorkflowStatus::Failed { .. }),
            "x: {x_status:?}"
        );
        assert!(
            matches!(y_status, WorkflowStatus::Failed { .. }),
            "y: {y_status:?}"
        );
        // At least one of them should mention "cycle" — the other may
        // carry a propagated dep-failure reason.
        let xr = match &x_status {
            WorkflowStatus::Failed { reason } => reason.clone(),
            _ => unreachable!(),
        };
        let yr = match &y_status {
            WorkflowStatus::Failed { reason } => reason.clone(),
            _ => unreachable!(),
        };
        assert!(
            xr.contains("cycle") || yr.contains("cycle"),
            "expected cycle reason on at least one; got x={xr}, y={yr}"
        );
    }

    // ── cascade-on-terminal (Phase 5.4.1) ──

    #[test]
    fn dependent_unblocks_when_dep_archives() {
        let (tmp, mut orch, mut rx) = fixture();

        // base: independent, will run to completion.
        write_tasks(&tmp, "base", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "base", "");
        // x: depends on base. Should sit Queued until base archives.
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");

        orch.handle_event(FsEvent::TasksModified { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        assert_eq!(
            orch.workflow("base").unwrap().status,
            WorkflowStatus::Implementing
        );
        assert_eq!(orch.workflow("x").unwrap().status, WorkflowStatus::Queued);

        let reqs = drain_requests(&mut rx);
        assert_eq!(reqs.len(), 1, "only base's apply prompt expected so far");
        let base_apply_tag = submit_tag(&reqs[0]).to_string();

        // Drive base apply → done.
        orch.handle_daemon_event(&fake_prompt_added(1, &base_apply_tag))
            .unwrap();
        write_tasks(&tmp, "base", "## 1. A\n- [x] 1.1 a\n");
        orch.handle_event(FsEvent::TasksModified { name: "base".into() })
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 1,
            exit_code: Some(0),
        })
        .unwrap();

        let verify_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(2, &verify_tag))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 2,
            exit_code: Some(0),
        })
        .unwrap();

        let archive_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(3, &archive_tag))
            .unwrap();
        // The WorkerFinished that archives `base` is the cascade trigger.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 3,
            exit_code: Some(0),
        })
        .unwrap();

        assert_eq!(
            orch.workflow("base").unwrap().status,
            WorkflowStatus::Archived
        );
        // Cascade should have advanced x into Implementing automatically.
        assert_eq!(
            orch.workflow("x").unwrap().status,
            WorkflowStatus::Implementing
        );
        let final_reqs = drain_requests(&mut rx);
        assert!(
            final_reqs
                .iter()
                .any(|r| submit_tag(r).contains("wf=x/phase=apply")),
            "expected cascade to dispatch x's apply prompt; got {final_reqs:#?}"
        );
    }

    #[test]
    fn dependent_fails_when_dep_fails_via_cascade() {
        let (tmp, mut orch, mut rx) = fixture();
        write_tasks(&tmp, "base", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "base", "");
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");

        orch.handle_event(FsEvent::TasksModified { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let base_apply_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(1, &base_apply_tag))
            .unwrap();
        // base apply exits non-zero → base fails.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 1,
            exit_code: Some(1),
        })
        .unwrap();

        assert!(matches!(
            orch.workflow("base").unwrap().status,
            WorkflowStatus::Failed { .. }
        ));
        // Cascade should have failed x with a dependency-failed reason.
        assert_failed(&orch, "x", "'base'");
    }

    #[test]
    fn cancel_workflow_cascades_failure_to_dependents() {
        let (tmp, mut orch, _rx) = fixture();
        // base is queued + running, x depends on it.
        write_tasks(&tmp, "base", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "base", "");
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");

        orch.handle_event(FsEvent::TasksModified { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        // Cancel base via the control-socket entry point. This goes
        // through `cancel_workflow_inner`, which now triggers the cascade.
        orch.cancel_workflow("base").unwrap();

        assert_eq!(
            orch.workflow("base").unwrap().status,
            WorkflowStatus::Cancelled
        );
        assert_failed(&orch, "x", "cancelled");
    }

    // ── blocked_by surfacing (Phase 5.4.2) ──

    #[test]
    fn summary_populates_blocked_by_for_pending_queued_workflow() {
        let (tmp, mut orch, _rx) = fixture();
        // base sits Drafted on disk → x stays Queued blocked.
        change_dir(&tmp, "base");
        write_tasks(&tmp, "base", "## 1. A\n- [ ] 1.1 a\n");

        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");
        orch.reconcile().unwrap();

        let s = orch.summary("x").expect("summary");
        assert_eq!(s.status, "queued");
        assert_eq!(s.blocked_by, vec!["base".to_string()]);
        // Detail mirrors the same field.
        let d = orch.detail("x").expect("detail");
        assert_eq!(d.blocked_by, vec!["base".to_string()]);
    }

    #[test]
    fn summary_blocked_by_is_empty_for_non_queued_states() {
        let (_tmp, mut orch, _rx) = fixture();
        // Inject a Drafted workflow with deps directly. Drafted is not
        // Queued, so blocked_by must stay empty regardless of dep state.
        let mut wf = Workflow::drafted("x");
        wf.metadata = MarkerMetadata {
            depends_on: vec!["ghost".into()],
            ..MarkerMetadata::default()
        };
        orch.store.save(&wf).unwrap();
        orch.reconcile().unwrap();

        let s = orch.summary("x").expect("summary");
        assert_eq!(s.status, "drafted");
        assert!(
            s.blocked_by.is_empty(),
            "Drafted workflow should not surface blocked_by, got {:?}",
            s.blocked_by
        );

        // Same for an Archived workflow (terminal).
        orch.store.save(&archived_workflow("done")).unwrap();
        orch.reconcile().unwrap();
        let s2 = orch.summary("done").expect("summary");
        assert_eq!(s2.status, "archived");
        assert!(s2.blocked_by.is_empty());
    }

    #[test]
    fn dep_clearing_emits_workflow_updated_with_shrunken_blocked_by() {
        let (tmp, mut orch, mut rx) = fixture();

        // base independent + x blocked on base.
        write_tasks(&tmp, "base", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "base", "");
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 a\n");
        write_marker(&tmp, "x", "depends_on = [\"base\"]\n");

        orch.handle_event(FsEvent::TasksModified { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "base".into() })
            .unwrap();
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        // x is Queued + blocked_by=["base"].
        assert_eq!(
            orch.summary("x").unwrap().blocked_by,
            vec!["base".to_string()]
        );

        // Subscribe *after* the initial setup so we only observe events
        // emitted by the cascade we're about to trigger.
        let mut events = orch.events_subscribe();

        // Drive base apply → verify → archive.
        let base_apply_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(1, &base_apply_tag))
            .unwrap();
        write_tasks(&tmp, "base", "## 1. A\n- [x] 1.1 a\n");
        orch.handle_event(FsEvent::TasksModified { name: "base".into() })
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 1,
            exit_code: Some(0),
        })
        .unwrap();

        let verify_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(2, &verify_tag))
            .unwrap();
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 2,
            exit_code: Some(0),
        })
        .unwrap();

        let archive_tag = submit_tag(&drain_requests(&mut rx)[0]).to_string();
        orch.handle_daemon_event(&fake_prompt_added(3, &archive_tag))
            .unwrap();
        // Final WorkerFinished archives base AND cascades x → Implementing.
        orch.handle_daemon_event(&DaemonEvent::WorkerFinished {
            prompt_id: 3,
            exit_code: Some(0),
        })
        .unwrap();

        // x's final state.
        let s = orch.summary("x").expect("summary");
        assert_eq!(s.status, "implementing");
        assert!(
            s.blocked_by.is_empty(),
            "blocked_by should clear once base is archived"
        );

        // Among the emitted events, find at least one for x with empty
        // blocked_by. The exact event count varies because the cascade
        // also emits for base + x's apply dispatch; the diff machinery
        // dedupes so we get one per genuine state shift, not per call.
        let emitted = drain_events(&mut events);
        let x_updates: Vec<_> = emitted
            .iter()
            .filter_map(|e| match e {
                SchedulerEvent::WorkflowUpdated { summary } if summary.name == "x" => {
                    Some(summary)
                }
                _ => None,
            })
            .collect();
        assert!(
            !x_updates.is_empty(),
            "expected at least one WorkflowUpdated for x in {emitted:#?}"
        );
        // The last x update should reflect the cleared blocked_by.
        let final_x = x_updates.last().unwrap();
        assert!(
            final_x.blocked_by.is_empty(),
            "final x update should have empty blocked_by, got {:?}",
            final_x.blocked_by
        );
    }
}
