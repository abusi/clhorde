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

use crate::dispatch::{is_node_done, next_runnable_nodes};
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

/// Errors surfaced from the orchestrator. We deliberately keep this small —
/// most callers want to log-and-continue, not to branch on the cause.
#[derive(Debug)]
pub enum OrchestratorError {
    Store(StoreError),
}

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrchestratorError::Store(e) => write!(f, "store: {e}"),
        }
    }
}

impl std::error::Error for OrchestratorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OrchestratorError::Store(e) => Some(e),
        }
    }
}

impl From<StoreError> for OrchestratorError {
    fn from(e: StoreError) -> Self {
        OrchestratorError::Store(e)
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
                self.note_prompt(info);
            }
            DaemonEvent::WorkerFinished {
                prompt_id,
                exit_code,
            } => {
                if let Some(name) = self.workflow_owning_prompt(*prompt_id) {
                    self.note_worker_finished(&name, *prompt_id, *exit_code);
                    self.try_advance(&name)?;
                }
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
}
