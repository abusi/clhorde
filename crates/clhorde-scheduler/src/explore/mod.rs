//! Explore-gate dispatch and lifecycle helpers.
//!
//! Section 5 of the `add-jira-source` change. The explore gate is the
//! "human-in-the-middle" beat between a non-OpenSpec source (today, Jira)
//! creating a workflow and a human writing `.clhorde-ready`. The
//! orchestrator drives the FSM (`Triggered → Exploring → {Queued,
//! Cancelled, Failed}`); this module owns the side-shaped pieces:
//!
//! - The literal `/opsx:explore` prompt template (per design D5: lives in
//!   source, not config).
//! - The substitution that turns a [`crate::jira::JiraTicketPayload`] into
//!   a concrete prompt body.
//! - The `ClientRequest::SubmitPrompt` builder that the orchestrator sends
//!   over the daemon-IPC outbound channel.
//! - The explore-tag string used to correlate the daemon's `PromptAdded`
//!   echo with the explore worker (mirrors the `phase=apply/verify/archive`
//!   convention).
//! - The idle-reaper helpers that decide which explore workers to kill and
//!   produce the matching `KillWorker` requests (see [`reap_idle_workers`]).
//!
//! Wiring lives in `orchestrator.rs`. Keeping the helper here means the
//! template, mode choice, and tag format are all exercised by unit tests
//! that don't require a full `Orchestrator` fixture.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use clhorde_core::protocol::ClientRequest;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::jira::JiraTicketPayload;
use crate::orchestrator::Orchestrator;

/// Phase suffix for explore worker tags. Combined with the workflow's
/// scheduler tag prefix yields e.g. `clhorde-scheduler/wf=PROJ-1/phase=explore`.
pub const EXPLORE_PHASE: &str = "explore";

/// Default idle threshold for the explore-worker reaper: 24 hours per
/// design D7. Concrete schedulers can override this at construction time
/// from `[sources.jira] idle_explore_timeout_secs`.
pub const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// The literal explore prompt template. Per design D5, this lives in
/// source code, not in a config file. A `keymap.toml` override hook can
/// land later if anyone needs it.
///
/// Substitutions are intentionally simple `{key}` placeholders: this
/// template is not user-authored, so the heavyweight Tera engine would
/// be overkill. The placeholders are also stable across the lifetime of
/// the change — renaming any of them is a breaking change to the
/// explore-gate contract.
const EXPLORE_TEMPLATE: &str = r#"/opsx:explore

You've been auto-spawned by the clhorde scheduler from a Jira ticket.
No human is here yet — they will attach via TUI or web shortly.

When a human arrives:
- Greet them, summarise what you understood from the ticket
- Ask the clarifying questions that emerge naturally
- When they signal you have enough to draft the change, create the
  proposal under `openspec/changes/{key}/`

The change directory MUST be named exactly `{key}` so the scheduler can
match it to the Jira ticket.

Until the human attaches, output a brief opening response acknowledging
the ticket and listing the clarifying questions you'd want answered. Do
NOT write any artifacts yet.

--- TICKET {key} ---
Title: {title}
Description: {description}
Acceptance Criteria: {acceptance_criteria}
Labels: {labels}
Reporter: {reporter}
"#;

/// Render the explore prompt for a Jira ticket payload.
///
/// Empty optional fields render as empty strings rather than `<missing>`
/// placeholders — the AI side handles "field is empty" naturally; we'd
/// rather not pollute the prompt with scheduler-vocabulary noise.
pub fn render_prompt(payload: &JiraTicketPayload) -> String {
    let labels = payload.labels.join(", ");
    let reporter = payload.reporter.as_deref().unwrap_or("");
    EXPLORE_TEMPLATE
        .replace("{key}", &payload.key)
        .replace("{title}", &payload.title)
        .replace("{description}", &payload.description)
        .replace("{acceptance_criteria}", &payload.acceptance_criteria)
        .replace("{labels}", &labels)
        .replace("{reporter}", reporter)
}

/// Build the explore tag for a workflow. The format mirrors
/// `phase=apply/verify/archive` so the orchestrator's existing tag
/// parser picks it up trivially when section 5.3 wires the dispatch.
pub fn explore_tag(workflow_name: &str) -> String {
    format!("clhorde-scheduler/wf={workflow_name}/phase={EXPLORE_PHASE}")
}

/// Errors raised by [`dispatch`].
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// The orchestrator's outbound channel to the daemon is closed.
    /// Surfaces to the caller as a catastrophic dispatch failure; per
    /// spec the workflow transitions to `Failed { reason }`.
    ChannelClosed,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::ChannelClosed => f.write_str("daemon outbound channel closed"),
        }
    }
}

impl std::error::Error for DispatchError {}

/// Build the `ClientRequest::SubmitPrompt` that dispatches the explore
/// worker. The mode is hard-coded to `"interactive"` (PTY) per spec.
///
/// `cwd` is the working directory the daemon should chdir into before
/// spawning the worker. The orchestrator passes its scheduler root so
/// the human's `openspec/changes/<KEY>/` writes land in the right repo.
pub fn build_request(
    workflow_name: &str,
    cwd: Option<String>,
    payload: &JiraTicketPayload,
) -> ClientRequest {
    ClientRequest::SubmitPrompt {
        text: render_prompt(payload),
        cwd,
        mode: "interactive".to_string(),
        // The explore worker writes directly to `openspec/changes/<KEY>/`
        // in the user's repo; running it in a worktree would defeat the
        // "drop a change dir, watcher picks it up" symmetry that D2
        // depends on.
        worktree: false,
        tags: vec![explore_tag(workflow_name)],
        depends_on: Vec::new(),
        worktree_id: None,
    }
}

/// Send the explore-worker dispatch over `outbound`. Returns
/// [`DispatchError::ChannelClosed`] if the daemon is gone.
pub fn dispatch(
    workflow_name: &str,
    cwd: Option<String>,
    payload: &JiraTicketPayload,
    outbound: &mpsc::UnboundedSender<ClientRequest>,
) -> Result<(), DispatchError> {
    let request = build_request(workflow_name, cwd, payload);
    outbound.send(request).map_err(|_| DispatchError::ChannelClosed)
}

/// Per-explore-worker bookkeeping. Held in the orchestrator alongside
/// the workflow runtime; updated as `PromptAdded` and human-input
/// signals arrive. The reaper walks every entry on each tick and
/// returns kill requests for those past the threshold.
#[derive(Debug, Clone)]
pub struct ExploreWorker {
    /// The workflow this worker belongs to (the Jira issue key).
    pub workflow: String,
    /// Daemon-assigned numeric prompt id. `None` until `PromptAdded`
    /// arrives, in which case the reaper skips the entry — we don't
    /// know how to kill a worker whose id is still pending.
    pub prompt_id: Option<usize>,
    /// Last time a human keystroke (or other input) hit this worker.
    /// Defaults to the dispatch time; bumped by the daemon-event
    /// handler when input lands.
    pub last_input_at: DateTime<Utc>,
}

impl ExploreWorker {
    /// Brand-new explore-worker entry, dispatched at `now`. The
    /// `prompt_id` is filled in later when the daemon echoes
    /// `PromptAdded`.
    pub fn new(workflow: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            workflow: workflow.into(),
            prompt_id: None,
            last_input_at: now,
        }
    }

    /// True when the worker has been idle past the configured
    /// threshold. Workers without a `prompt_id` (still mid-dispatch)
    /// are never considered idle — a kill request would be a no-op
    /// anyway and risks racing with the `PromptAdded` echo.
    pub fn is_idle_for(&self, now: DateTime<Utc>, threshold: Duration) -> bool {
        if self.prompt_id.is_none() {
            return false;
        }
        let elapsed = now.signed_duration_since(self.last_input_at);
        // chrono's signed Duration converts to std::Duration only for
        // non-negative spans; clock skew that produces a negative
        // elapsed must not flag the worker as idle.
        match elapsed.to_std() {
            Ok(d) => d >= threshold,
            Err(_) => false,
        }
    }
}

/// Default cadence between reaper sweeps. The reaper is cheap (it
/// walks an in-memory map of usually-zero entries), so the cadence is
/// driven more by responsiveness than by cost: a 60-second tick means
/// a 24-hour idle threshold can over-shoot by at most one minute.
pub const DEFAULT_REAPER_INTERVAL: Duration = Duration::from_secs(60);

/// Handle returned by [`spawn_reaper`]. Dropping it does NOT cancel
/// the reaper — the daemon binary keeps it alive until shutdown and
/// uses [`ReaperHandle::shutdown`] for an explicit teardown in tests.
pub struct ReaperHandle {
    join: JoinHandle<()>,
}

impl ReaperHandle {
    /// Cancel the reaper task and await its termination. Production
    /// code drops the handle on daemon shutdown; tests use this so
    /// the reaper doesn't outlive the runtime.
    pub async fn shutdown(self) {
        self.join.abort();
        let _ = self.join.await;
    }
}

/// Spawn the long-lived idle-reaper task. Each tick locks the
/// orchestrator briefly to run [`Orchestrator::reap_idle_explore_workers`];
/// nothing else lives in the lock window. The reaper is best-effort —
/// a failed kill request lands in the daemon log and the entry is
/// already gone from the orchestrator's map regardless.
///
/// Per spec the reaper kills the worker but does NOT touch FSM state.
/// `Orchestrator::reap_idle_explore_workers` enforces that contract;
/// this helper is just the timer plumbing.
pub fn spawn_reaper(
    orch: Arc<Mutex<Orchestrator>>,
    interval: Duration,
    threshold: Duration,
) -> ReaperHandle {
    let join = tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            // Brief lock window: do the sweep, drop the lock before
            // sleeping again.
            let killed = {
                let mut guard = match orch.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.reap_idle_explore_workers(Utc::now(), threshold)
            };
            if killed > 0 {
                tracing::info!(count = killed, "explore reaper killed idle workers");
            }
        }
    });
    ReaperHandle { join }
}

/// Walk every explore worker and return the `KillWorker` requests for
/// those past `threshold`. The caller is expected to send each request
/// over the daemon-IPC channel and remove the corresponding worker
/// from its in-memory map; the helper itself is pure so it's
/// deterministic in tests.
///
/// The reaper does NOT touch the workflow's FSM state — per spec the
/// reap is a worker-only operation. The workflow stays in `Exploring`;
/// the human can re-dispatch via `clhorde-cli explore <id>`.
pub fn reap_idle_workers<'a>(
    workers: impl IntoIterator<Item = &'a ExploreWorker>,
    now: DateTime<Utc>,
    threshold: Duration,
) -> Vec<ClientRequest> {
    workers
        .into_iter()
        .filter(|w| w.is_idle_for(now, threshold))
        .filter_map(|w| {
            w.prompt_id
                .map(|id| ClientRequest::KillWorker { prompt_id: id })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> JiraTicketPayload {
        JiraTicketPayload {
            key: "PROJ-1".into(),
            title: "Add OAuth login".into(),
            description: "We need OAuth so users can sign in via Google.".into(),
            acceptance_criteria: "- Google OAuth\n- Apple OAuth".into(),
            labels: vec!["clhorde-plan".into(), "auth".into()],
            reporter: Some("alice".into()),
        }
    }

    #[test]
    fn template_substitutes_every_placeholder() {
        let body = render_prompt(&payload());
        // No leftover placeholders.
        assert!(!body.contains("{key}"), "key placeholder leaked: {body}");
        assert!(!body.contains("{title}"));
        assert!(!body.contains("{description}"));
        assert!(!body.contains("{acceptance_criteria}"));
        assert!(!body.contains("{labels}"));
        assert!(!body.contains("{reporter}"));
        // Field bodies present.
        assert!(body.contains("PROJ-1"));
        assert!(body.contains("Add OAuth login"));
        assert!(body.contains("We need OAuth so users"));
        assert!(body.contains("- Google OAuth"));
        assert!(body.contains("clhorde-plan, auth"));
        assert!(body.contains("alice"));
    }

    #[test]
    fn template_carries_change_name_directive() {
        // Spec scenario: "Prompt template carries the change-name
        // directive". The body must instruct the AI to use the issue
        // key as the change directory name.
        let body = render_prompt(&payload());
        assert!(
            body.contains("openspec/changes/PROJ-1"),
            "expected change-name directive referencing PROJ-1; got: {body}"
        );
        assert!(
            body.contains("MUST be named exactly `PROJ-1`"),
            "expected MUST directive on change-dir name"
        );
    }

    #[test]
    fn template_starts_with_opsx_explore_invocation() {
        // Drift guard: D5 ties us to `/opsx:explore`. If anyone ever
        // edits the template they need to keep this directive at the
        // top.
        let body = render_prompt(&payload());
        assert!(
            body.starts_with("/opsx:explore"),
            "explore template must invoke /opsx:explore at the top; got: {body}"
        );
    }

    #[test]
    fn missing_optional_fields_render_as_empty_strings() {
        let p = JiraTicketPayload {
            key: "X-1".into(),
            title: "T".into(),
            description: String::new(),
            acceptance_criteria: String::new(),
            labels: Vec::new(),
            reporter: None,
        };
        let body = render_prompt(&p);
        // No placeholders left even though every optional is empty.
        assert!(!body.contains('{'));
        // `Reporter:` line still present, just blank.
        assert!(body.contains("Reporter: \n"));
        assert!(body.contains("Labels: \n"));
    }

    #[test]
    fn explore_tag_format_matches_phase_convention() {
        assert_eq!(
            explore_tag("PROJ-1"),
            "clhorde-scheduler/wf=PROJ-1/phase=explore"
        );
    }

    #[test]
    fn build_request_is_interactive_and_carries_explore_tag() {
        let req = build_request("PROJ-1", Some("/repo".into()), &payload());
        match req {
            ClientRequest::SubmitPrompt {
                mode,
                worktree,
                tags,
                depends_on,
                worktree_id,
                cwd,
                text,
            } => {
                assert_eq!(mode, "interactive");
                assert!(!worktree, "explore worker must run outside worktrees");
                assert_eq!(tags, vec!["clhorde-scheduler/wf=PROJ-1/phase=explore"]);
                assert!(depends_on.is_empty());
                assert!(worktree_id.is_none());
                assert_eq!(cwd.as_deref(), Some("/repo"));
                assert!(text.starts_with("/opsx:explore"));
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_sends_one_request_through_outbound() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        dispatch("PROJ-1", None, &payload(), &tx).expect("dispatch ok");
        let req = rx.try_recv().expect("one request emitted");
        match req {
            ClientRequest::SubmitPrompt { mode, .. } => {
                assert_eq!(mode, "interactive");
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
        // No further requests.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_returns_channel_closed_when_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let err = dispatch("PROJ-1", None, &payload(), &tx).unwrap_err();
        assert_eq!(err, DispatchError::ChannelClosed);
    }

    #[test]
    fn worker_without_prompt_id_is_never_idle() {
        let now = Utc::now();
        let mut w = ExploreWorker::new("PROJ-1", now);
        // last_input_at far in the past, but no prompt_id → not idle.
        w.last_input_at = now - chrono::Duration::hours(48);
        assert!(!w.is_idle_for(now, DEFAULT_IDLE_THRESHOLD));
    }

    #[test]
    fn worker_idle_past_threshold_is_flagged() {
        let now = Utc::now();
        let mut w = ExploreWorker::new("PROJ-1", now);
        w.prompt_id = Some(7);
        w.last_input_at = now - chrono::Duration::hours(25);
        assert!(w.is_idle_for(now, DEFAULT_IDLE_THRESHOLD));
    }

    #[test]
    fn worker_active_within_threshold_is_not_idle() {
        let now = Utc::now();
        let mut w = ExploreWorker::new("PROJ-1", now);
        w.prompt_id = Some(7);
        w.last_input_at = now - chrono::Duration::hours(1);
        assert!(!w.is_idle_for(now, DEFAULT_IDLE_THRESHOLD));
    }

    #[test]
    fn reap_emits_kill_workers_only_for_idle_with_id() {
        let now = Utc::now();
        let stale = ExploreWorker {
            workflow: "PROJ-1".into(),
            prompt_id: Some(11),
            last_input_at: now - chrono::Duration::hours(48),
        };
        let fresh = ExploreWorker {
            workflow: "PROJ-2".into(),
            prompt_id: Some(22),
            last_input_at: now,
        };
        let no_id = ExploreWorker {
            workflow: "PROJ-3".into(),
            prompt_id: None,
            last_input_at: now - chrono::Duration::hours(72),
        };

        let kills = reap_idle_workers(
            [&stale, &fresh, &no_id].iter().copied(),
            now,
            DEFAULT_IDLE_THRESHOLD,
        );
        assert_eq!(kills.len(), 1);
        match &kills[0] {
            ClientRequest::KillWorker { prompt_id } => {
                assert_eq!(*prompt_id, 11);
            }
            other => panic!("expected KillWorker, got {other:?}"),
        }
    }

    #[test]
    fn reap_with_zero_threshold_kills_every_worker_with_id() {
        let now = Utc::now();
        let a = ExploreWorker {
            workflow: "PROJ-1".into(),
            prompt_id: Some(1),
            last_input_at: now,
        };
        let b = ExploreWorker {
            workflow: "PROJ-2".into(),
            prompt_id: Some(2),
            last_input_at: now,
        };
        let kills = reap_idle_workers([&a, &b].iter().copied(), now, Duration::ZERO);
        assert_eq!(kills.len(), 2);
    }
}
