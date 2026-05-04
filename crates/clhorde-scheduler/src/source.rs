//! Multi-source plumbing for the orchestrator.
//!
//! [`SourceEvent`] is the unified dispatch type the orchestrator's event
//! handler accepts. A source produces concrete events (e.g.
//! [`crate::watcher::FsEvent`] for OpenSpec, [`crate::jira::JiraEvent`] for
//! Jira) and wraps them in `SourceEvent` before forwarding. The
//! orchestrator branches on the variant once and then runs the same
//! per-workflow advance logic regardless of origin.
//!
//! `SourceHealth` is the per-source observability surface: every source
//! reports `last_successful_run`, `last_error`, and a derived
//! `is_healthy` flag. The control socket includes these in the daemon's
//! status response so CLI/TUI/web can render them next to the workflow
//! list without per-source plumbing.
//!
//! Spec coverage: `scheduler-source` capability — single dispatch entry
//! point (Requirement 2), source health (Requirement 3), per-source
//! independence (Requirement 5).

use chrono::{DateTime, Utc};

pub use clhorde_core::control::SourceHealthReport;

use crate::jira::JiraEvent;
use crate::watcher::FsEvent;
use crate::workflow::SourceKind;

/// Unified event type consumed by [`crate::orchestrator::Orchestrator::handle_source_event`].
///
/// Both the OpenSpec watcher and the Jira poll loop wrap their concrete
/// events in this enum; the orchestrator branches on the variant inside
/// its `_inner` helper. The wrapping is cheap (no allocation, just a
/// discriminant) and keeps the call sites uniform.
#[derive(Debug, Clone)]
pub enum SourceEvent {
    /// Filesystem event from the OpenSpec watcher.
    Fs(FsEvent),
    /// Polling event from the Jira source.
    Jira(JiraEvent),
}

impl From<FsEvent> for SourceEvent {
    fn from(e: FsEvent) -> Self {
        SourceEvent::Fs(e)
    }
}

impl From<JiraEvent> for SourceEvent {
    fn from(e: JiraEvent) -> Self {
        SourceEvent::Jira(e)
    }
}

impl SourceEvent {
    /// Which source produced this event. Used by the orchestrator to
    /// stamp [`SourceHealth::last_successful_run`] on the right entry
    /// after dispatch.
    pub fn origin(&self) -> SourceKind {
        match self {
            SourceEvent::Fs(_) => SourceKind::OpenSpec,
            SourceEvent::Jira(_) => SourceKind::Jira,
        }
    }
}

/// Observability snapshot for one source.
///
/// `last_successful_run` advances every time the orchestrator processes
/// an event from this source without an error. `last_error` is the most
/// recent error message; when set and unrelieved by a subsequent
/// success, `is_healthy` is `false`. A source that has never produced
/// an event reports `last_successful_run = None`, `last_error = None`,
/// `is_healthy = true` (treated as healthy-but-quiet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceHealth {
    pub last_successful_run: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub is_healthy: bool,
}

impl SourceHealth {
    /// Brand-new source: never run, no error, optimistically healthy.
    pub fn unstarted() -> Self {
        Self {
            last_successful_run: None,
            last_error: None,
            is_healthy: true,
        }
    }

    /// Mark a successful run at `at`. Clears any previous error and
    /// flips `is_healthy` back to true.
    pub fn note_success(&mut self, at: DateTime<Utc>) {
        self.last_successful_run = Some(at);
        self.last_error = None;
        self.is_healthy = true;
    }

    /// Mark an error. Leaves `last_successful_run` alone so the
    /// monitoring surface can still reason about how stale the last
    /// success is.
    pub fn note_error(&mut self, message: impl Into<String>) {
        self.last_error = Some(message.into());
        self.is_healthy = false;
    }
}

/// Convert the in-memory [`SourceHealth`] to the wire shape, stamping
/// the source name to match the persisted [`SourceKind`] serialisation.
pub fn report(kind: SourceKind, health: &SourceHealth) -> SourceHealthReport {
    SourceHealthReport {
        source: source_kind_name(kind).to_string(),
        last_successful_run: health.last_successful_run,
        last_error: health.last_error.clone(),
        is_healthy: health.is_healthy,
    }
}

/// Lower-case wire name for a [`SourceKind`]. Matches how serde renders
/// the enum (`#[serde(rename_all = "snake_case")]`).
pub fn source_kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::OpenSpec => "open_spec",
        SourceKind::Jira => "jira",
    }
}

/// Startup helper that wires every active source into the orchestrator
/// and the unified [`SourceEvent`] channel.
///
/// Today only the OpenSpec watcher is wired through this helper. The
/// Jira poll loop will register itself the same way once section 4 of
/// `add-jira-source` lands; the helper exists now so both sources have
/// one place to register and so [`SourceHealth`] entries for unstarted
/// sources can be surfaced before any event has landed.
///
/// `register` is generic over the orchestrator-state container so the
/// daemon binary (which holds an `Arc<Mutex<Orchestrator>>`) and unit
/// tests (which hold an `Orchestrator` directly) can share one path.
pub fn register_default_sources(
    orch: &mut crate::orchestrator::Orchestrator,
    enable_jira: bool,
) {
    orch.register_source(SourceKind::OpenSpec);
    if enable_jira {
        orch.register_source(SourceKind::Jira);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jira::JiraTicketPayload;

    #[test]
    fn fs_event_origin_is_openspec() {
        let ev: SourceEvent = FsEvent::MarkerCreated {
            name: "x".into(),
        }
        .into();
        assert_eq!(ev.origin(), SourceKind::OpenSpec);
    }

    #[test]
    fn jira_event_origin_is_jira() {
        let ev: SourceEvent = JiraEvent::TicketLeftFilter {
            key: "PROJ-1".into(),
        }
        .into();
        assert_eq!(ev.origin(), SourceKind::Jira);
    }

    #[test]
    fn source_health_note_success_clears_error() {
        let mut h = SourceHealth::unstarted();
        h.note_error("boom");
        assert!(!h.is_healthy);
        h.note_success(Utc::now());
        assert!(h.is_healthy);
        assert!(h.last_error.is_none());
    }

    #[test]
    fn source_health_note_error_keeps_last_success() {
        let mut h = SourceHealth::unstarted();
        let t = Utc::now();
        h.note_success(t);
        h.note_error("boom");
        assert!(!h.is_healthy);
        assert_eq!(h.last_successful_run, Some(t));
    }

    #[test]
    fn jira_payload_round_trips() {
        let p = JiraTicketPayload {
            key: "PROJ-1".into(),
            title: "Add OAuth".into(),
            description: "Body".into(),
            acceptance_criteria: "AC".into(),
            labels: vec!["clhorde-plan".into()],
            reporter: Some("alice".into()),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: JiraTicketPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn source_health_report_round_trips() {
        let r = SourceHealthReport {
            source: "jira".into(),
            last_successful_run: Some(Utc::now()),
            last_error: Some("boom".into()),
            is_healthy: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: SourceHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
