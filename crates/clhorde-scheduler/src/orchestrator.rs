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
use tokio::sync::mpsc;

use crate::control::WorkflowSummary;
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
}

impl Orchestrator {
    pub fn new(
        root: impl Into<PathBuf>,
        store: WorkflowStore,
        outbound: mpsc::UnboundedSender<ClientRequest>,
    ) -> Self {
        let root = root.into();
        let templates = TemplateEngine::new(&root);
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
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reconcile in-memory state with what's on disk. Idempotent — call at
    /// startup *and* whenever the FS or the store may have drifted.
    pub fn reconcile(&mut self) -> Result<(), OrchestratorError> {
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
            if let Err(e) = self.try_advance(&n) {
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
        self.workflows.values().map(workflow_summary).collect()
    }

    /// Snapshot one workflow as a [`WorkflowSummary`], or `None` if it
    /// does not exist.
    pub fn summary(&self, name: &str) -> Option<WorkflowSummary> {
        self.workflows.get(name).map(workflow_summary)
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
        self.on_marker_removed(name.to_string())?;
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
        self.try_advance(&name)
    }

    /// Process one [`DaemonEvent`] from the upstream daemon. Only the
    /// events the scheduler cares about are handled — everything else is a
    /// no-op so unrelated traffic doesn't pollute the workflow state.
    pub fn handle_daemon_event(
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
                    self.try_advance(&name)?;
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
    /// from any external trigger.
    pub fn try_advance(&mut self, name: &str) -> Result<(), OrchestratorError> {
        let Some(mut wf) = self.workflows.remove(name) else {
            return Ok(());
        };
        let result = self.advance_inner(&mut wf);
        // Save and reinsert regardless of error so we don't lose the
        // workflow on a transient failure.
        if let Err(e) = self.store.save(&wf) {
            tracing::warn!(name = %wf.name, error = %e, "persisting after advance failed");
        }
        self.workflows.insert(name.to_string(), wf);
        result
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
fn workflow_summary(wf: &Workflow) -> WorkflowSummary {
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
    WorkflowSummary {
        name: wf.name.clone(),
        status: status.to_string(),
        failure_reason,
        priority: wf.metadata.priority.unwrap_or(0),
        queued_at: wf.queued_at,
        started_at: wf.started_at,
        finished_at: wf.finished_at,
        prompt_ids: wf.prompt_ids.clone(),
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
}
