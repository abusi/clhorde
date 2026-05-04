//! In-memory representation of one scheduler workflow plus its state machine.
//!
//! A workflow is the scheduler-side counterpart of an OpenSpec change. It
//! moves through a small finite state machine driven by external events
//! (marker created/removed, prompts finishing, user cancellation). Transition
//! functions return `Result<(), TransitionError>` so invalid moves surface
//! loudly instead of silently no-oping.
//!
//! ```text
//! Drafted ──queue──▶ Queued ──start_implementing──▶ Implementing
//!                       ▲                              │
//!                       │                              ▼
//!                       │                          Verifying
//!                       │                              │
//!                       │                              ▼
//!                       │                          Archiving
//!                       │                              │
//!                       │                              ▼
//!                       │                           Archived
//!                       │
//!                       │  approval (MarkerCreated)
//! Triggered ──start_exploring──▶ Exploring ──────────┘
//!  (Jira)                          │
//!                                  └─cancel_from_exploring──▶ Cancelled
//!
//!  Drafted ──unqueue──▶ Drafted
//!
//!  any_active ──cancel──▶ Cancelled
//!  any_active ──fail───▶  Failed { reason }
//! ```
//!
//! `dag` is intentionally not stored here — it is rebuilt from `tasks.md` on
//! demand and may be unavailable (e.g. after a scheduler restart before the
//! next file read). Persistence (Phase 2.2's `persistence.rs`) only writes
//! the durable fields.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::openspec::discovery::MarkerMetadata;

/// Where a workflow is in its lifecycle.
///
/// `Default` is `Drafted` because a workflow loaded from disk before any
/// status is recorded should fall back to the safest possible state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowStatus {
    /// User is still iterating; no marker on disk.
    #[default]
    Drafted,
    /// A non-OpenSpec source (e.g. Jira) has created the workflow but the
    /// explore-gate worker hasn't been dispatched yet. Transient — the
    /// orchestrator immediately calls `start_exploring()` after a successful
    /// dispatch.
    Triggered,
    /// Explore-gate worker is alive (or has exited and is resumable). The
    /// workflow is parked here until a human writes `.clhorde-ready` or
    /// rejects via `clhorde-cli reject`.
    Exploring,
    /// Marker on disk; scheduler will pick up when workers free up.
    Queued,
    /// Apply phase prompts in flight.
    Implementing,
    /// Apply done; verifying step running.
    Verifying,
    /// Verify done; archive step running.
    Archiving,
    /// Done; the change has been archived by OpenSpec.
    Archived,
    /// User cancelled (e.g. removed the marker mid-flight).
    Cancelled,
    /// A step failed past its retry budget.
    Failed { reason: String },
}

/// Which source created a workflow. Persisted alongside the workflow so the
/// orchestrator can route lifecycle write-back to the right place (Jira
/// comments, label management, …) without re-deriving origin from the name.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// The OpenSpec watcher saw a change directory. This is the legacy
    /// path and the default for back-compat with workflows persisted before
    /// `source` was a field.
    #[default]
    OpenSpec,
    /// The Jira poll loop ingested an issue matching a configured JQL
    /// filter and used the issue key as the workflow name.
    Jira,
}

impl WorkflowStatus {
    /// Returns true if the workflow is in a terminal state — no further
    /// transitions are valid. `Exploring` is intentionally **not** terminal
    /// even when its worker has exited: a marker write or CLI signal can
    /// still move it forward or cancel it.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            WorkflowStatus::Archived | WorkflowStatus::Cancelled | WorkflowStatus::Failed { .. }
        )
    }

    /// Returns true if the scheduler is actively working on this workflow's
    /// implementation phases. `Exploring` is **not** "running" in this
    /// sense — it's a human-gated parking phase, distinct from the
    /// OpenSpec apply/verify/archive cadence the rest of the scheduler
    /// reasons about.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            WorkflowStatus::Implementing
                | WorkflowStatus::Verifying
                | WorkflowStatus::Archiving
        )
    }
}

/// Raised when a state transition is attempted from a state that does not
/// allow it. The error carries enough context to render a useful message but
/// should otherwise be rare in normal operation — callers usually only need
/// to log it and move on.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionError {
    pub from: WorkflowStatus,
    pub attempted: &'static str,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot {} from {:?}",
            self.attempted, self.from
        )
    }
}

impl std::error::Error for TransitionError {}

/// One scheduler workflow. Owns its current status, optional marker
/// metadata, and bookkeeping timestamps. The DAG is not persisted; it is
/// rebuilt from `tasks.md` on demand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    #[serde(default)]
    pub status: WorkflowStatus,
    #[serde(default)]
    pub metadata: MarkerMetadata,
    /// UUIDs of prompts dispatched on this workflow's behalf, in chronological
    /// order. Phase 2.4 starts populating it.
    #[serde(default)]
    pub prompt_ids: Vec<String>,
    /// First time this workflow was seen by the scheduler (any state).
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Time the marker was first observed on disk.
    #[serde(default)]
    pub queued_at: Option<DateTime<Utc>>,
    /// Time `start_implementing` ran.
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// Time the workflow reached a terminal state.
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// Which source created the workflow. Defaults to [`SourceKind::OpenSpec`]
    /// for backward compatibility with workflows persisted before this
    /// field existed.
    #[serde(default)]
    pub source: SourceKind,
}

impl Workflow {
    /// Build a fresh `Drafted` workflow. Use [`Workflow::queued`] when the
    /// caller already saw a marker on disk.
    pub fn drafted(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: WorkflowStatus::Drafted,
            metadata: MarkerMetadata::default(),
            prompt_ids: Vec::new(),
            created_at: Some(Utc::now()),
            queued_at: None,
            started_at: None,
            finished_at: None,
            source: SourceKind::OpenSpec,
        }
    }

    /// Build a workflow that is already `Queued` — convenience for
    /// startup reconciliation.
    pub fn queued(name: impl Into<String>, metadata: MarkerMetadata) -> Self {
        let now = Utc::now();
        Self {
            name: name.into(),
            status: WorkflowStatus::Queued,
            metadata,
            prompt_ids: Vec::new(),
            created_at: Some(now),
            queued_at: Some(now),
            started_at: None,
            finished_at: None,
            source: SourceKind::OpenSpec,
        }
    }

    /// Build a workflow in the transient `Triggered` state. Used by the
    /// Jira source the moment it sees a new ticket and before it has
    /// dispatched the explore worker. Callers should immediately call
    /// [`Workflow::start_exploring`] once dispatch succeeds, or
    /// [`Workflow::fail`] if dispatch fails catastrophically.
    pub fn triggered(name: impl Into<String>, source: SourceKind) -> Self {
        Self {
            name: name.into(),
            status: WorkflowStatus::Triggered,
            metadata: MarkerMetadata::default(),
            prompt_ids: Vec::new(),
            created_at: Some(Utc::now()),
            queued_at: None,
            started_at: None,
            finished_at: None,
            source,
        }
    }

    /// Move from `Drafted` or `Exploring` to `Queued`.
    ///
    /// `Drafted → Queued` is the OpenSpec watcher's marker-created path.
    /// `Exploring → Queued` is the explore-gate approval path: the human
    /// has written `.clhorde-ready` (directly or via `clhorde-cli approve`)
    /// and the orchestrator drives the same handler regardless of source.
    pub fn queue(&mut self, metadata: MarkerMetadata) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Drafted | WorkflowStatus::Exploring => {
                self.status = WorkflowStatus::Queued;
                self.metadata = metadata;
                self.queued_at = Some(Utc::now());
                Ok(())
            }
            _ => Err(self.bad_transition("queue")),
        }
    }

    /// Move from `Triggered` to `Exploring`. Called by the explore-gate
    /// dispatch helper after it has successfully spawned the interactive
    /// worker; the workflow then parks here until a human approves
    /// (marker write → `queue()`) or rejects (CLI signal →
    /// [`Workflow::cancel_from_exploring`]).
    pub fn start_exploring(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Triggered => {
                self.status = WorkflowStatus::Exploring;
                Ok(())
            }
            _ => Err(self.bad_transition("start_exploring")),
        }
    }

    /// Cancel from `Exploring` specifically. The generic `cancel()` method
    /// also accepts `Exploring`, but this dedicated entry point exists to
    /// make explore-gate call-sites self-documenting and to give the
    /// scheduler one place to audit Jira-side reject side-effects.
    pub fn cancel_from_exploring(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Exploring => {
                self.status = WorkflowStatus::Cancelled;
                self.finished_at = Some(Utc::now());
                Ok(())
            }
            _ => Err(self.bad_transition("cancel_from_exploring")),
        }
    }

    /// Move from `Queued` back to `Drafted` (marker removed before pickup).
    pub fn unqueue(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Queued => {
                self.status = WorkflowStatus::Drafted;
                self.queued_at = None;
                self.metadata = MarkerMetadata::default();
                Ok(())
            }
            _ => Err(self.bad_transition("unqueue")),
        }
    }

    /// Move from `Queued` to `Implementing`.
    pub fn start_implementing(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Queued => {
                self.status = WorkflowStatus::Implementing;
                self.started_at = Some(Utc::now());
                Ok(())
            }
            _ => Err(self.bad_transition("start_implementing")),
        }
    }

    /// Move from `Implementing` to `Verifying`.
    pub fn start_verifying(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Implementing => {
                self.status = WorkflowStatus::Verifying;
                Ok(())
            }
            _ => Err(self.bad_transition("start_verifying")),
        }
    }

    /// Move from `Verifying` to `Archiving`.
    pub fn start_archiving(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Verifying => {
                self.status = WorkflowStatus::Archiving;
                Ok(())
            }
            _ => Err(self.bad_transition("start_archiving")),
        }
    }

    /// Move from `Archiving` to `Archived` (terminal).
    pub fn archive(&mut self) -> Result<(), TransitionError> {
        match self.status {
            WorkflowStatus::Archiving => {
                self.status = WorkflowStatus::Archived;
                self.finished_at = Some(Utc::now());
                Ok(())
            }
            _ => Err(self.bad_transition("archive")),
        }
    }

    /// Cancel from any non-terminal state. The watcher calls this when a
    /// `.clhorde-ready` marker is removed mid-flight.
    pub fn cancel(&mut self) -> Result<(), TransitionError> {
        if self.status.is_terminal() {
            return Err(self.bad_transition("cancel"));
        }
        self.status = WorkflowStatus::Cancelled;
        self.finished_at = Some(Utc::now());
        Ok(())
    }

    /// Move into `Failed { reason }` from any non-terminal state. The reason
    /// string is shown back to the user in `status`.
    pub fn fail(&mut self, reason: impl Into<String>) -> Result<(), TransitionError> {
        if self.status.is_terminal() {
            return Err(self.bad_transition("fail"));
        }
        self.status = WorkflowStatus::Failed {
            reason: reason.into(),
        };
        self.finished_at = Some(Utc::now());
        Ok(())
    }

    fn bad_transition(&self, attempted: &'static str) -> TransitionError {
        TransitionError {
            from: self.status.clone(),
            attempted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_with_priority(p: i32) -> MarkerMetadata {
        MarkerMetadata {
            priority: Some(p),
            ..MarkerMetadata::default()
        }
    }

    // ── happy path ──

    #[test]
    fn full_lifecycle_succeeds() {
        let mut wf = Workflow::drafted("add-oauth");
        wf.queue(meta_with_priority(1)).unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert!(wf.queued_at.is_some());

        wf.start_implementing().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Implementing);
        assert!(wf.started_at.is_some());

        wf.start_verifying().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Verifying);

        wf.start_archiving().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Archiving);

        wf.archive().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Archived);
        assert!(wf.finished_at.is_some());
        assert!(wf.status.is_terminal());
    }

    #[test]
    fn unqueue_returns_to_drafted_and_clears_metadata() {
        let mut wf = Workflow::drafted("x");
        wf.queue(meta_with_priority(9)).unwrap();
        wf.unqueue().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Drafted);
        assert_eq!(wf.metadata, MarkerMetadata::default());
        assert!(wf.queued_at.is_none());
    }

    #[test]
    fn cancel_works_from_every_running_state() {
        for transitions in [
            vec![],
            vec!["queue"],
            vec!["queue", "impl"],
            vec!["queue", "impl", "verify"],
            vec!["queue", "impl", "verify", "archiving"],
        ] {
            let mut wf = Workflow::drafted("x");
            for t in &transitions {
                match *t {
                    "queue" => wf.queue(MarkerMetadata::default()).unwrap(),
                    "impl" => wf.start_implementing().unwrap(),
                    "verify" => wf.start_verifying().unwrap(),
                    "archiving" => wf.start_archiving().unwrap(),
                    _ => unreachable!(),
                }
            }
            wf.cancel().unwrap();
            assert_eq!(wf.status, WorkflowStatus::Cancelled);
            assert!(wf.status.is_terminal());
        }
    }

    #[test]
    fn fail_records_reason() {
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        wf.fail("section 2 retried out").unwrap();
        match &wf.status {
            WorkflowStatus::Failed { reason } => {
                assert_eq!(reason, "section 2 retried out");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(wf.finished_at.is_some());
    }

    // ── illegal transitions ──

    #[test]
    fn cannot_queue_while_running() {
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        let err = wf.queue(MarkerMetadata::default()).unwrap_err();
        assert_eq!(err.attempted, "queue");
        assert_eq!(err.from, WorkflowStatus::Implementing);
    }

    #[test]
    fn cannot_unqueue_unless_queued() {
        let mut wf = Workflow::drafted("x");
        let err = wf.unqueue().unwrap_err();
        assert_eq!(err.attempted, "unqueue");
        assert_eq!(err.from, WorkflowStatus::Drafted);
    }

    #[test]
    fn cannot_archive_from_implementing() {
        // Skipping verifying isn't allowed.
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        let err = wf.archive().unwrap_err();
        assert_eq!(err.attempted, "archive");
        assert_eq!(err.from, WorkflowStatus::Implementing);
    }

    #[test]
    fn cannot_transition_from_terminal_state() {
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        wf.start_verifying().unwrap();
        wf.start_archiving().unwrap();
        wf.archive().unwrap();

        assert!(wf.cancel().is_err());
        assert!(wf.fail("late").is_err());
        assert!(wf.queue(MarkerMetadata::default()).is_err());
    }

    #[test]
    fn cannot_cancel_when_already_failed() {
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.fail("oops").unwrap();
        let err = wf.cancel().unwrap_err();
        assert!(matches!(err.from, WorkflowStatus::Failed { .. }));
    }

    // ── status helpers ──

    #[test]
    fn is_running_distinguishes_active_phases() {
        assert!(!WorkflowStatus::Drafted.is_running());
        assert!(!WorkflowStatus::Triggered.is_running());
        assert!(!WorkflowStatus::Exploring.is_running());
        assert!(!WorkflowStatus::Queued.is_running());
        assert!(WorkflowStatus::Implementing.is_running());
        assert!(WorkflowStatus::Verifying.is_running());
        assert!(WorkflowStatus::Archiving.is_running());
        assert!(!WorkflowStatus::Archived.is_running());
        assert!(!WorkflowStatus::Cancelled.is_running());
        assert!(
            !WorkflowStatus::Failed {
                reason: "x".into()
            }
            .is_running()
        );
    }

    #[test]
    fn is_terminal_excludes_triggered_and_exploring() {
        // Explicit per scheduler-workflow spec: Exploring is a parking
        // phase, not a terminal state. Triggered is transient.
        assert!(!WorkflowStatus::Triggered.is_terminal());
        assert!(!WorkflowStatus::Exploring.is_terminal());
        assert!(WorkflowStatus::Archived.is_terminal());
        assert!(WorkflowStatus::Cancelled.is_terminal());
        assert!(
            WorkflowStatus::Failed {
                reason: "x".into()
            }
            .is_terminal()
        );
    }

    // ── constructors ──

    #[test]
    fn queued_constructor_stamps_both_timestamps() {
        let wf = Workflow::queued("x", meta_with_priority(2));
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert!(wf.created_at.is_some());
        assert!(wf.queued_at.is_some());
        assert_eq!(wf.metadata.priority, Some(2));
    }

    #[test]
    fn drafted_defaults_to_openspec_source() {
        let wf = Workflow::drafted("x");
        assert_eq!(wf.source, SourceKind::OpenSpec);
    }

    #[test]
    fn triggered_constructor_records_source_and_state() {
        let wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        assert_eq!(wf.status, WorkflowStatus::Triggered);
        assert_eq!(wf.source, SourceKind::Jira);
        assert!(wf.created_at.is_some());
        assert!(wf.queued_at.is_none());
        assert!(wf.started_at.is_none());
        assert!(wf.finished_at.is_none());
    }

    // ── Triggered → Exploring → {Queued, Cancelled} ──

    #[test]
    fn start_exploring_promotes_triggered() {
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Exploring);
    }

    #[test]
    fn cannot_start_exploring_outside_triggered() {
        for ctor in [
            || Workflow::drafted("x"),
            || Workflow::queued("x", MarkerMetadata::default()),
        ] {
            let mut wf = ctor();
            let err = wf.start_exploring().unwrap_err();
            assert_eq!(err.attempted, "start_exploring");
        }
        // From Implementing, also rejected.
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        let err = wf.start_exploring().unwrap_err();
        assert_eq!(err.attempted, "start_exploring");
        assert_eq!(err.from, WorkflowStatus::Implementing);
    }

    #[test]
    fn approval_promotes_exploring_to_queued() {
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        wf.queue(meta_with_priority(3)).unwrap();
        assert_eq!(wf.status, WorkflowStatus::Queued);
        assert_eq!(wf.metadata.priority, Some(3));
        assert!(wf.queued_at.is_some());
    }

    #[test]
    fn cancel_from_exploring_terminates() {
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        wf.cancel_from_exploring().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Cancelled);
        assert!(wf.status.is_terminal());
        assert!(wf.finished_at.is_some());
    }

    #[test]
    fn cancel_from_exploring_rejects_other_states() {
        let mut wf = Workflow::drafted("x");
        let err = wf.cancel_from_exploring().unwrap_err();
        assert_eq!(err.attempted, "cancel_from_exploring");
        assert_eq!(err.from, WorkflowStatus::Drafted);

        let mut wf2 = Workflow::triggered("PROJ-1", SourceKind::Jira);
        let err = wf2.cancel_from_exploring().unwrap_err();
        assert_eq!(err.from, WorkflowStatus::Triggered);
    }

    #[test]
    fn generic_cancel_also_works_from_exploring() {
        // The spec lists both `cancel_from_exploring` and the general
        // `cancel` as valid; the latter is what `RejectRequested` /
        // `TicketLeftFilter` handlers fall through to.
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        wf.cancel().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Cancelled);
    }

    #[test]
    fn cannot_skip_exploring_directly_to_implementing() {
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        let err = wf.start_implementing().unwrap_err();
        assert_eq!(err.attempted, "start_implementing");
        assert_eq!(err.from, WorkflowStatus::Exploring);
    }

    #[test]
    fn cannot_unqueue_from_exploring() {
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        let err = wf.unqueue().unwrap_err();
        assert_eq!(err.attempted, "unqueue");
        assert_eq!(err.from, WorkflowStatus::Exploring);
    }

    #[test]
    fn cannot_archive_or_verify_from_exploring() {
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        assert!(wf.start_verifying().is_err());
        assert!(wf.start_archiving().is_err());
        assert!(wf.archive().is_err());
    }

    #[test]
    fn fail_works_from_triggered_and_exploring() {
        // Catastrophic dispatch failure path → Failed.
        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.fail("dispatch refused").unwrap();
        assert!(matches!(wf.status, WorkflowStatus::Failed { .. }));

        let mut wf2 = Workflow::triggered("PROJ-2", SourceKind::Jira);
        wf2.start_exploring().unwrap();
        wf2.fail("worker crashed unrecoverably").unwrap();
        assert!(matches!(wf2.status, WorkflowStatus::Failed { .. }));
    }

    #[test]
    fn cannot_re_enter_exploring_from_implementing() {
        // Spec scenario: "Cannot enter Exploring from Implementing".
        // We don't have a public transition into Exploring from
        // Implementing; the only entry is `start_exploring()` from
        // Triggered. Verify the latter doesn't accidentally accept
        // Implementing.
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        let err = wf.start_exploring().unwrap_err();
        assert_eq!(err.from, WorkflowStatus::Implementing);
    }
}
