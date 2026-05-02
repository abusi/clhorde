//! Glue layer between filesystem events and persisted workflow state.
//!
//! The orchestrator is the single owner of the in-memory workflow map plus
//! the [`WorkflowStore`] on disk. External signals (FS watcher events, CLI
//! commands, scheduler restart) flow through [`Orchestrator::handle_event`]
//! / [`Orchestrator::reconcile`] and produce state-machine transitions on
//! the [`Workflow`]s.
//!
//! Phase 2.3 stops at "state machine + persistence". Prompt dispatch,
//! `depends_on` wiring, and the actual `tasks.md → DAG` translation happen
//! in Phase 2.4. The cached parsed tasks (see [`Orchestrator::tasks_for`])
//! are the bridge: 2.3 keeps them up to date so 2.4 can read them when
//! deciding which sections to fire next.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::openspec::annotations::{annotate, AnnotatedSection};
use crate::openspec::discovery::{self, ChangeStatus, MarkerMetadata};
use crate::openspec::tasks_parser;
use crate::persistence::{StoreError, WorkflowStore};
use crate::watcher::FsEvent;
use crate::workflow::{Workflow, WorkflowStatus};

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

pub struct Orchestrator {
    root: PathBuf,
    store: WorkflowStore,
    workflows: BTreeMap<String, Workflow>,
    /// Last successfully parsed tasks for each workflow. Rebuilt on every
    /// `TasksModified` and on reconcile. Phase 2.4 uses this to drive the
    /// DAG. Empty when `tasks.md` is missing or unparseable.
    parsed_tasks: BTreeMap<String, Vec<AnnotatedSection>>,
}

impl Orchestrator {
    pub fn new(root: impl Into<PathBuf>, store: WorkflowStore) -> Self {
        Self {
            root: root.into(),
            store,
            workflows: BTreeMap::new(),
            parsed_tasks: BTreeMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reconcile in-memory state with what's on disk. Idempotent — call
    /// at startup *and* whenever the FS or the store may have drifted (e.g.
    /// after losing a notify connection).
    ///
    /// Behaviour:
    /// 1. Load every persisted workflow.
    /// 2. Walk `<root>/openspec/changes/*` once.
    /// 3. For each discovered change, fold the on-disk marker state into the
    ///    workflow:
    ///    - missing → existing-with-marker: `queue`.
    ///    - `Drafted` workflow + marker now present: `queue`.
    ///    - Persisted as `Queued` but marker now gone: `unqueue` (or
    ///      `cancel` if it had already started).
    /// 4. Re-parse `tasks.md` for every change that has one.
    /// 5. Persist any workflow whose status changed.
    pub fn reconcile(&mut self) -> Result<(), OrchestratorError> {
        // Pull the whole store into memory.
        for wf in self.store.list()? {
            self.workflows.insert(wf.name.clone(), wf);
        }

        let discovered = discovery::scan(&self.root);
        for change in &discovered {
            self.reconcile_change(change)?;
            self.refresh_tasks(&change.name);
        }

        Ok(())
    }

    /// Borrow every workflow we know about, sorted by name.
    pub fn workflows(&self) -> impl Iterator<Item = &Workflow> {
        self.workflows.values()
    }

    pub fn workflow(&self, name: &str) -> Option<&Workflow> {
        self.workflows.get(name)
    }

    /// Latest parsed sections for a workflow's `tasks.md`. Empty slice if the
    /// file is missing or did not parse cleanly.
    pub fn tasks_for(&self, name: &str) -> &[AnnotatedSection] {
        self.parsed_tasks
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Process one [`FsEvent`]. Pure state mutation + persistence — does not
    /// dispatch prompts (Phase 2.4).
    pub fn handle_event(&mut self, event: FsEvent) -> Result<(), OrchestratorError> {
        match event {
            FsEvent::MarkerCreated { name } => self.on_marker_created(name)?,
            FsEvent::MarkerRemoved { name } => self.on_marker_removed(name)?,
            FsEvent::TasksModified { name } => {
                self.refresh_tasks(&name);
            }
        }
        Ok(())
    }

    // ── private ──

    fn reconcile_change(
        &mut self,
        change: &discovery::DiscoveredChange,
    ) -> Result<(), OrchestratorError> {
        match (&change.status, self.workflows.remove(&change.name)) {
            // Marker on disk, no persisted workflow → fresh queued workflow.
            (ChangeStatus::Queued(meta), None) => {
                let wf = Workflow::queued(&change.name, meta.clone());
                self.store.save(&wf)?;
                self.workflows.insert(change.name.clone(), wf);
            }
            // Marker on disk, persisted workflow → bring it forward if needed.
            (ChangeStatus::Queued(meta), Some(mut wf)) => {
                let changed = match wf.status {
                    WorkflowStatus::Drafted => {
                        // This must succeed — Drafted is the only entry to
                        // queue. If it fails the workflow is broken; surface
                        // it as a fail() and persist.
                        if let Err(e) = wf.queue(meta.clone()) {
                            tracing::warn!(name = %change.name, error = %e, "reconcile queue failed");
                            let _ = wf.fail(format!("reconcile queue: {e}"));
                        }
                        true
                    }
                    WorkflowStatus::Queued => {
                        // Marker still present; refresh the metadata in case
                        // the user edited it but keep status.
                        if wf.metadata != *meta {
                            wf.metadata = meta.clone();
                            true
                        } else {
                            false
                        }
                    }
                    // Already running / verifying / etc. — nothing to do.
                    _ => false,
                };
                if changed {
                    self.store.save(&wf)?;
                }
                self.workflows.insert(change.name.clone(), wf);
            }
            // No marker on disk, no persisted workflow → it's a draft.
            (ChangeStatus::Drafted, None) => {
                let wf = Workflow::drafted(&change.name);
                self.store.save(&wf)?;
                self.workflows.insert(change.name.clone(), wf);
            }
            // No marker, but we had a persisted workflow → maybe the user
            // pulled the marker while we were down. Reflect that.
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
                        // Editing the marker while queued just refreshes
                        // metadata.
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
        // No workflow yet for this name — a stray remove event with no prior
        // create. Silently ignore.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Orchestrator) {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("store"));
        let orch = Orchestrator::new(tmp.path(), store);
        (tmp, orch)
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

    // ── handle_event ──

    #[test]
    fn marker_created_creates_queued_workflow() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "add-oauth", "priority = 5\n");

        orch.handle_event(FsEvent::MarkerCreated {
            name: "add-oauth".into(),
        })
        .unwrap();

        let wf = orch.workflow("add-oauth").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata.priority, Some(5));
        // And it's persisted.
        let loaded = orch.store.load("add-oauth").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Queued);
    }

    #[test]
    fn marker_removed_unqueues_a_queued_workflow() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        remove_marker(&tmp, "x");
        orch.handle_event(FsEvent::MarkerRemoved { name: "x".into() })
            .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Drafted);
        assert!(wf.queued_at.is_none());
    }

    #[test]
    fn marker_removed_cancels_running_workflow() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        // Force the workflow forward into Implementing — Phase 2.4 will do
        // this in production via prompt dispatch.
        {
            let wf = orch.workflows.get_mut("x").unwrap();
            wf.start_implementing().unwrap();
        }

        remove_marker(&tmp, "x");
        orch.handle_event(FsEvent::MarkerRemoved { name: "x".into() })
            .unwrap();

        assert_eq!(orch.workflow("x").unwrap().status, WorkflowStatus::Cancelled);
        // Persisted too.
        let loaded = orch.store.load("x").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Cancelled);
    }

    #[test]
    fn marker_remove_for_unknown_workflow_is_silently_ignored() {
        let (_tmp, mut orch) = fixture();
        // No workflow for "ghost". Should not panic and should not create one.
        orch.handle_event(FsEvent::MarkerRemoved {
            name: "ghost".into(),
        })
        .unwrap();
        assert!(orch.workflow("ghost").is_none());
    }

    #[test]
    fn marker_create_then_re_create_refreshes_metadata_only() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "priority = 1\n");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();
        let queued_at_first = orch.workflow("x").unwrap().queued_at;

        // Edit marker in place.
        write_marker(&tmp, "x", "priority = 9\n");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata.priority, Some(9));
        // queued_at is preserved on metadata edit.
        assert_eq!(wf.queued_at, queued_at_first);
    }

    #[test]
    fn tasks_modified_caches_parsed_sections() {
        let (tmp, mut orch) = fixture();
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
        assert_eq!(sections[0].section.id, "1");
        assert!(!sections[0].items[0].task.done);
        assert!(sections[1].items[0].task.done);
    }

    #[test]
    fn tasks_modified_updates_done_state_on_re_parse() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 first\n");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        assert!(!orch.tasks_for("x")[0].items[0].task.done);

        // User (or worker) ticks the box and we re-parse.
        write_tasks(&tmp, "x", "## 1. A\n- [x] 1.1 first\n");
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        assert!(orch.tasks_for("x")[0].items[0].task.done);
    }

    #[test]
    fn missing_tasks_md_clears_cache_silently() {
        let (tmp, mut orch) = fixture();
        change_dir(&tmp, "x");
        // No tasks.md on disk.
        orch.handle_event(FsEvent::TasksModified { name: "x".into() })
            .unwrap();
        assert!(orch.tasks_for("x").is_empty());
    }

    #[test]
    fn malformed_marker_falls_back_to_defaults() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "this isn't [valid TOML");
        orch.handle_event(FsEvent::MarkerCreated { name: "x".into() })
            .unwrap();

        let wf = orch.workflow("x").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata, MarkerMetadata::default());
    }

    // ── reconcile / restart ──

    #[test]
    fn reconcile_picks_up_existing_marker() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "add-oauth", "priority = 3\n");

        orch.reconcile().unwrap();

        let wf = orch.workflow("add-oauth").unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata.priority, Some(3));
    }

    #[test]
    fn reconcile_unqueues_when_marker_disappeared() {
        let (tmp, mut orch1) = fixture();
        write_marker(&tmp, "x", "");
        orch1.reconcile().unwrap();
        assert_eq!(
            orch1.workflow("x").unwrap().status,
            WorkflowStatus::Queued
        );

        // Simulate restart: marker removed while scheduler was offline.
        remove_marker(&tmp, "x");
        let store = orch1.store.clone();
        let mut orch2 = Orchestrator::new(tmp.path(), store);
        orch2.reconcile().unwrap();

        assert_eq!(
            orch2.workflow("x").unwrap().status,
            WorkflowStatus::Drafted
        );
    }

    #[test]
    fn reconcile_cancels_when_running_workflow_lost_its_marker() {
        let (tmp, mut orch1) = fixture();
        write_marker(&tmp, "x", "");
        orch1.reconcile().unwrap();
        // Force-advance to Implementing as if Phase 2.4 had started running.
        {
            let wf = orch1.workflows.get_mut("x").unwrap();
            wf.start_implementing().unwrap();
            orch1.store.save(wf).unwrap();
        }

        // User pulls the marker while the scheduler is offline.
        remove_marker(&tmp, "x");
        let store = orch1.store.clone();
        let mut orch2 = Orchestrator::new(tmp.path(), store);
        orch2.reconcile().unwrap();

        assert_eq!(
            orch2.workflow("x").unwrap().status,
            WorkflowStatus::Cancelled
        );
    }

    #[test]
    fn reconcile_keeps_drafts_drafted() {
        let (tmp, mut orch) = fixture();
        change_dir(&tmp, "draft-only");
        orch.reconcile().unwrap();
        assert_eq!(
            orch.workflow("draft-only").unwrap().status,
            WorkflowStatus::Drafted
        );
    }

    #[test]
    fn reconcile_caches_tasks_md() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "");
        write_tasks(&tmp, "x", "## 1. A\n- [ ] 1.1 q\n");

        orch.reconcile().unwrap();
        assert_eq!(orch.tasks_for("x").len(), 1);
    }

    #[test]
    fn reconcile_is_idempotent() {
        let (tmp, mut orch) = fixture();
        write_marker(&tmp, "x", "priority = 1\n");
        orch.reconcile().unwrap();
        let wf_before = orch.workflow("x").unwrap().clone();

        orch.reconcile().unwrap();
        let wf_after = orch.workflow("x").unwrap();
        // queued_at and metadata stable across calls.
        assert_eq!(wf_after.queued_at, wf_before.queued_at);
        assert_eq!(wf_after.metadata, wf_before.metadata);
    }

    #[test]
    fn reconcile_refreshes_metadata_when_marker_was_edited_offline() {
        let (tmp, mut orch1) = fixture();
        write_marker(&tmp, "x", "priority = 1\n");
        orch1.reconcile().unwrap();

        // Edit marker while scheduler was offline.
        write_marker(&tmp, "x", "priority = 99\n");

        let store = orch1.store.clone();
        let mut orch2 = Orchestrator::new(tmp.path(), store);
        orch2.reconcile().unwrap();

        let wf = orch2.workflow("x").unwrap();
        assert_eq!(wf.metadata.priority, Some(99));
        assert_eq!(wf.status, WorkflowStatus::Queued);
    }
}
