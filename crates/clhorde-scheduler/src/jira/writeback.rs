//! Jira write-back: comments, label removal, optional status transitions.
//!
//! Section 6 of the `add-jira-source` change. The orchestrator decides
//! the FSM advance synchronously (Triggered → Exploring, archive,
//! cancel) and then asks this module to mirror the lifecycle event back
//! to Jira. Write-back is fire-and-forget by design — per spec
//! `jira-source` Requirement 5 and `scheduler-source` Requirement 5,
//! Jira API failures must NOT block or delay the workflow's local state
//! machine.
//!
//! ## Three classes, independently togglable (per design D8)
//!
//! - **comments** (`comments` flag, default on) — short status comments
//!   on the ticket: "🤖 clhorde started exploring this", "🤖 finished",
//!   "🤖 cancelled".
//! - **labels** (`labels` flag, default on) — remove the trigger label
//!   when the workflow leaves the explore gate, so the next poll
//!   doesn't re-create a workflow for the same ticket.
//! - **transitions** (`transitions` map, default empty / off) — Jira
//!   status transitions on workflow state changes, mapped per lifecycle
//!   phase.
//!
//! Each class is a separate guard inside the same spawned task so
//! disabling one (e.g. `transitions` empty) skips just that call —
//! comments and labels still go out.
//!
//! ## Fire-and-forget shape
//!
//! Every public `notify_*` method:
//! 1. Spawns one [`tokio::spawn`] task.
//! 2. Returns immediately to the caller.
//! 3. Inside the task, runs each enabled write-back call sequentially
//!    (so per-key ordering is preserved on Jira's side).
//! 4. On any failure, logs a `tracing::warn!` and forwards the
//!    formatted message through the optional `error_tx` channel — the
//!    daemon binary owns the receiver and pumps it into
//!    [`crate::orchestrator::Orchestrator::record_source_error`] so
//!    `last_jira_error` lights up on the source-health surface.
//!
//! Tests can drop `error_tx` and read errors directly through the
//! receiver they own.
//!
//! ## Why a writer trait, not `JiraClient` directly?
//!
//! Same reason as `IssueSearch` in [`crate::jira::source`]: tests need
//! a synthetic implementation that records calls and can be scripted to
//! return errors without spinning up a `wiremock::MockServer`. The real
//! [`crate::jira::JiraClient`] gets a blanket impl.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::client::JiraClient;
use super::error::JiraError;

/// Default trigger label removed when a workflow leaves `Exploring`.
/// Mirrors the spec example (`clhorde-plan`); section 8 will wire a
/// per-queue override through config.
pub const DEFAULT_TRIGGER_LABEL: &str = "clhorde-plan";

/// Default comment text on each lifecycle phase. Concrete strings live
/// here (not in config) for the same reason the explore template does:
/// the surface is small enough that one source-of-truth string per
/// phase is easier to audit than a Tera template.
pub const COMMENT_EXPLORING: &str = "🤖 clhorde started exploring this";
pub const COMMENT_ARCHIVED: &str = "🤖 clhorde finished — change archived";
pub const COMMENT_CANCELLED: &str = "🤖 clhorde cancelled this";

/// Lifecycle phase the orchestrator is reporting back to Jira. Used as
/// the key in [`JiraWritebackConfig::transitions`] so a queue can wire
/// "Exploring → In Progress" / "Archived → In Review" without naming
/// the underlying FSM transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LifecyclePhase {
    /// `Triggered → Exploring`: the explore worker just spawned.
    Exploring,
    /// `Archiving → Archived`: the workflow finished cleanly.
    Archived,
    /// Any non-terminal → `Cancelled`: reject, ticket-left-filter, or
    /// manual cancel.
    Cancelled,
}

/// Runtime config for a Jira source's write-back behaviour.
///
/// Section 8 will populate this from `[sources.jira.queues.<name>]`;
/// section 6 only needs the strongly-typed shape so the orchestrator
/// can build it programmatically.
#[derive(Debug, Clone)]
pub struct JiraWritebackConfig {
    /// Whether to post comments on lifecycle events (`Exploring`,
    /// `Archived`, `Cancelled`). Default on per spec
    /// `jira-source` Requirement 4.
    pub comments: bool,
    /// Whether to remove the trigger label on `Exploring` start. Default
    /// on per spec.
    pub labels: bool,
    /// Trigger label removed when the workflow enters `Exploring`. The
    /// poll loop's JQL is what selected the ticket in the first place;
    /// removing this label is what keeps the next poll from re-creating
    /// the workflow.
    pub trigger_label: String,
    /// Per-phase transition ids. An empty map disables transitions
    /// entirely (default per spec `jira-source` Requirement 4).
    /// Each entry maps a [`LifecyclePhase`] to the Jira transition id
    /// to apply when the workflow enters that phase.
    pub transitions: BTreeMap<LifecyclePhase, String>,
    /// Comment body for [`LifecyclePhase::Exploring`]. Defaults to
    /// [`COMMENT_EXPLORING`]; exposed so a future config layer can
    /// override per-queue without touching this module.
    pub comment_exploring: String,
    /// Comment body for [`LifecyclePhase::Archived`].
    pub comment_archived: String,
    /// Comment body for [`LifecyclePhase::Cancelled`].
    pub comment_cancelled: String,
}

impl Default for JiraWritebackConfig {
    fn default() -> Self {
        Self {
            comments: true,
            labels: true,
            trigger_label: DEFAULT_TRIGGER_LABEL.to_string(),
            transitions: BTreeMap::new(),
            comment_exploring: COMMENT_EXPLORING.to_string(),
            comment_archived: COMMENT_ARCHIVED.to_string(),
            comment_cancelled: COMMENT_CANCELLED.to_string(),
        }
    }
}

impl JiraWritebackConfig {
    fn comment_for(&self, phase: LifecyclePhase) -> &str {
        match phase {
            LifecyclePhase::Exploring => &self.comment_exploring,
            LifecyclePhase::Archived => &self.comment_archived,
            LifecyclePhase::Cancelled => &self.comment_cancelled,
        }
    }
}

/// Abstraction over the three write operations the orchestrator needs.
/// `JiraClient` gets a blanket impl below; tests pass a synthetic
/// implementation that records calls and can be scripted to fail.
///
/// Boxed futures (rather than `async fn` in a trait) because the
/// orchestrator stores the writer behind an `Arc<dyn JiraWriter>` so it
/// can be shared across spawned tasks cheaply.
pub trait JiraWriter: Send + Sync + 'static {
    fn add_comment<'a>(
        &'a self,
        key: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>>;

    fn remove_label<'a>(
        &'a self,
        key: &'a str,
        label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>>;

    fn transition<'a>(
        &'a self,
        key: &'a str,
        transition_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>>;
}

impl JiraWriter for JiraClient {
    fn add_comment<'a>(
        &'a self,
        key: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>> {
        Box::pin(async move { JiraClient::add_comment(self, key, body).await })
    }

    fn remove_label<'a>(
        &'a self,
        key: &'a str,
        label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>> {
        Box::pin(async move { JiraClient::remove_label(self, key, label).await })
    }

    fn transition<'a>(
        &'a self,
        key: &'a str,
        transition_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>> {
        Box::pin(async move { JiraClient::transition(self, key, transition_id).await })
    }
}

/// Fire-and-forget Jira write-back driver.
///
/// Cheap to clone — wraps the writer behind `Arc` and the config behind
/// `Arc` too, so the spawned tasks can keep their own handles without
/// taking the orchestrator's lock.
///
/// The orchestrator holds an `Option<Arc<JiraWriteback>>` and calls
/// `notify_*` at the lifecycle hooks (start_exploring, archive, cancel
/// for Jira-source workflows). Production wiring lives in the daemon
/// binary — section 8's config layer is what builds the `Arc` and
/// passes it in.
pub struct JiraWriteback {
    writer: Arc<dyn JiraWriter>,
    config: Arc<JiraWritebackConfig>,
    error_tx: Option<mpsc::UnboundedSender<String>>,
    in_flight: Arc<AtomicUsize>,
}

impl JiraWriteback {
    /// Build a writeback driver. `error_tx` may be `None` if the caller
    /// doesn't care about surfacing failures (e.g. the simplest tests);
    /// production wiring always supplies one so `last_jira_error` can
    /// reflect failures.
    pub fn new(
        writer: Arc<dyn JiraWriter>,
        config: JiraWritebackConfig,
        error_tx: Option<mpsc::UnboundedSender<String>>,
    ) -> Self {
        Self {
            writer,
            config: Arc::new(config),
            error_tx,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of write-back tasks currently running. Tests call this
    /// (via [`Self::wait_idle`]) to synchronise with fire-and-forget
    /// completion; production code does not need it.
    pub fn pending(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Park the current task until every spawned write-back has
    /// completed. Test-only synchronisation primitive — production code
    /// must remain decoupled from in-flight write-backs.
    ///
    /// Polls every 10ms; the writeback's per-call latency is dominated
    /// by network so finer polling buys nothing.
    pub async fn wait_idle(&self) {
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Notify Jira that workflow `key` entered `Exploring`. Fires
    /// (in this order, when their flags allow):
    /// 1. Comment ([`COMMENT_EXPLORING`] by default)
    /// 2. Trigger-label removal
    /// 3. Optional status transition mapped to
    ///    [`LifecyclePhase::Exploring`].
    pub fn notify_exploring(&self, key: impl Into<String>) {
        self.spawn_phase(key.into(), LifecyclePhase::Exploring, true);
    }

    /// Notify Jira that workflow `key` reached `Archived`. Fires the
    /// closing comment and the optional `Archived` transition.
    /// Label removal is skipped — by the time the workflow archives,
    /// the trigger label was already removed at `Exploring` start.
    pub fn notify_archived(&self, key: impl Into<String>) {
        self.spawn_phase(key.into(), LifecyclePhase::Archived, false);
    }

    /// Notify Jira that workflow `key` was cancelled (reject,
    /// ticket-left-filter, or manual cancel). Fires the cancellation
    /// comment, removes the trigger label (best-effort — re-poll
    /// resilience: if the human un-rejects in Jira, the workflow can
    /// be re-triggered cleanly), and applies the optional `Cancelled`
    /// transition.
    pub fn notify_cancelled(&self, key: impl Into<String>) {
        // Cancel paths also remove the trigger label — without it, a
        // ticket re-entering the filter post-cancel would re-create
        // the same workflow, which is surprising.
        self.spawn_phase(key.into(), LifecyclePhase::Cancelled, true);
    }

    fn spawn_phase(&self, key: String, phase: LifecyclePhase, remove_label: bool) {
        let writer = Arc::clone(&self.writer);
        let config = Arc::clone(&self.config);
        let error_tx = self.error_tx.clone();
        let in_flight = Arc::clone(&self.in_flight);
        in_flight.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            // Decrement no matter how the body exits. A panic inside
            // the task would still drop the guard and keep `pending`
            // accurate.
            struct Guard(Arc<AtomicUsize>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _guard = Guard(in_flight);

            // 1. Comment.
            if config.comments {
                let body = config.comment_for(phase);
                if let Err(e) = writer.add_comment(&key, body).await {
                    record_failure(
                        &error_tx,
                        format!("jira comment on {key} ({phase:?}): {e}"),
                    );
                }
            }

            // 2. Label removal (Exploring start + Cancelled paths only).
            if remove_label && config.labels {
                if let Err(e) = writer
                    .remove_label(&key, &config.trigger_label)
                    .await
                {
                    record_failure(
                        &error_tx,
                        format!(
                            "jira label remove on {key} ({label}): {e}",
                            label = config.trigger_label
                        ),
                    );
                }
            }

            // 3. Optional status transition.
            if let Some(transition_id) = config.transitions.get(&phase).cloned() {
                if let Err(e) = writer.transition(&key, &transition_id).await {
                    record_failure(
                        &error_tx,
                        format!(
                            "jira transition on {key} ({phase:?} → {transition_id}): {e}"
                        ),
                    );
                }
            }
        });
    }
}

fn record_failure(error_tx: &Option<mpsc::UnboundedSender<String>>, message: String) {
    tracing::warn!(error = %message, "jira write-back failed");
    if let Some(tx) = error_tx {
        // If the receiver is gone the daemon is shutting down or
        // never wired one up — drop the message silently. The
        // tracing line above is enough for forensics.
        let _ = tx.send(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Synthetic writer that records every call and can be scripted to
    /// return failures on specific operations.
    struct RecordingWriter {
        comments: Mutex<Vec<(String, String)>>,
        labels_removed: Mutex<Vec<(String, String)>>,
        transitions: Mutex<Vec<(String, String)>>,
        fail_comments: Mutex<bool>,
        fail_labels: Mutex<bool>,
    }

    impl RecordingWriter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                comments: Mutex::new(Vec::new()),
                labels_removed: Mutex::new(Vec::new()),
                transitions: Mutex::new(Vec::new()),
                fail_comments: Mutex::new(false),
                fail_labels: Mutex::new(false),
            })
        }

        fn fail_next_comments(&self) {
            *self.fail_comments.lock().unwrap() = true;
        }

        fn fail_next_labels(&self) {
            *self.fail_labels.lock().unwrap() = true;
        }
    }

    impl JiraWriter for RecordingWriter {
        fn add_comment<'a>(
            &'a self,
            key: &'a str,
            body: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>> {
            Box::pin(async move {
                self.comments
                    .lock()
                    .unwrap()
                    .push((key.to_string(), body.to_string()));
                if *self.fail_comments.lock().unwrap() {
                    Err(JiraError::Server {
                        status: 500,
                        body: "synthetic".into(),
                    })
                } else {
                    Ok(())
                }
            })
        }

        fn remove_label<'a>(
            &'a self,
            key: &'a str,
            label: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>> {
            Box::pin(async move {
                self.labels_removed
                    .lock()
                    .unwrap()
                    .push((key.to_string(), label.to_string()));
                if *self.fail_labels.lock().unwrap() {
                    Err(JiraError::Network("synthetic".into()))
                } else {
                    Ok(())
                }
            })
        }

        fn transition<'a>(
            &'a self,
            key: &'a str,
            transition_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>> {
            Box::pin(async move {
                self.transitions
                    .lock()
                    .unwrap()
                    .push((key.to_string(), transition_id.to_string()));
                Ok(())
            })
        }
    }

    fn writeback_with(
        writer: Arc<RecordingWriter>,
        config: JiraWritebackConfig,
    ) -> (JiraWriteback, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            JiraWriteback::new(writer as Arc<dyn JiraWriter>, config, Some(tx)),
            rx,
        )
    }

    #[tokio::test]
    async fn exploring_posts_comment_and_removes_label() {
        let writer = RecordingWriter::new();
        let (wb, _rx) = writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_exploring("PROJ-1");
        wb.wait_idle().await;

        let comments = writer.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].0, "PROJ-1");
        assert_eq!(comments[0].1, COMMENT_EXPLORING);

        let labels = writer.labels_removed.lock().unwrap().clone();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0], ("PROJ-1".into(), DEFAULT_TRIGGER_LABEL.into()));

        // Default transitions map is empty → no transition call.
        assert!(writer.transitions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn comments_disabled_means_no_chatter() {
        // Spec scenario `jira-source` Requirement 4 / Scenario "Comments
        // disabled means no Jira chatter": with `comments = false`, no
        // comment is posted on any lifecycle phase.
        let writer = RecordingWriter::new();
        let mut config = JiraWritebackConfig::default();
        config.comments = false;
        let (wb, _rx) = writeback_with(Arc::clone(&writer), config);

        wb.notify_exploring("PROJ-1");
        wb.notify_archived("PROJ-1");
        wb.notify_cancelled("PROJ-1");
        wb.wait_idle().await;

        assert!(writer.comments.lock().unwrap().is_empty());
        // labels still removed (independent flag).
        assert_eq!(writer.labels_removed.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn labels_disabled_skips_label_removal() {
        let writer = RecordingWriter::new();
        let mut config = JiraWritebackConfig::default();
        config.labels = false;
        let (wb, _rx) = writeback_with(Arc::clone(&writer), config);

        wb.notify_exploring("PROJ-1");
        wb.wait_idle().await;

        assert_eq!(writer.comments.lock().unwrap().len(), 1);
        assert!(writer.labels_removed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn transitions_disabled_means_no_transition_call() {
        // Spec scenario `jira-source` Requirement 4 / Scenario "Status
        // transition only when explicitly enabled": with the default
        // empty `transitions` map, no transition request is issued
        // even when the workflow archives.
        let writer = RecordingWriter::new();
        let (wb, _rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_exploring("PROJ-1");
        wb.notify_archived("PROJ-1");
        wb.notify_cancelled("PROJ-1");
        wb.wait_idle().await;

        assert!(writer.transitions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn transitions_enabled_apply_per_phase() {
        let writer = RecordingWriter::new();
        let mut config = JiraWritebackConfig::default();
        config
            .transitions
            .insert(LifecyclePhase::Exploring, "31".into());
        config
            .transitions
            .insert(LifecyclePhase::Archived, "61".into());
        let (wb, _rx) = writeback_with(Arc::clone(&writer), config);

        wb.notify_exploring("PROJ-1");
        wb.notify_archived("PROJ-1");
        // Cancelled has no transition entry → no transition for it.
        wb.notify_cancelled("PROJ-2");
        wb.wait_idle().await;

        let mut t = writer.transitions.lock().unwrap().clone();
        t.sort();
        assert_eq!(
            t,
            vec![
                ("PROJ-1".to_string(), "31".to_string()),
                ("PROJ-1".to_string(), "61".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn comment_failure_surfaces_via_error_channel() {
        // Spec scenario `jira-source` Requirement 5 / Scenario "Comment
        // fails on archive": a 500 from Jira is logged + surfaced via
        // `last_jira_error` (modelled here as the receiver) and does
        // NOT propagate as an Err to the caller.
        let writer = RecordingWriter::new();
        writer.fail_next_comments();
        let (wb, mut rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_archived("PROJ-1");
        wb.wait_idle().await;

        let msg = rx.try_recv().expect("error must be surfaced");
        assert!(msg.contains("PROJ-1"), "got {msg}");
        assert!(msg.contains("Archived"), "got {msg}");
        // No further errors queued (label removal skipped on archive).
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn comment_fail_does_not_block_label_removal() {
        // Verifies fire-and-forget independence: a failing comment
        // does not abort the rest of the spawned task.
        let writer = RecordingWriter::new();
        writer.fail_next_comments();
        let (wb, _rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_exploring("PROJ-1");
        wb.wait_idle().await;

        assert_eq!(
            writer.labels_removed.lock().unwrap().len(),
            1,
            "label remove must run even after comment failure"
        );
    }

    #[tokio::test]
    async fn label_remove_is_best_effort() {
        // Spec scenario `explore-gate` Requirement 4 / Scenario "Reject
        // succeeds even if Jira write-back fails": label-removal
        // failures are logged but do not poison the rest of the
        // pipeline.
        let writer = RecordingWriter::new();
        writer.fail_next_labels();
        let (wb, mut rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_exploring("PROJ-1");
        wb.wait_idle().await;

        // Comment did go out.
        assert_eq!(writer.comments.lock().unwrap().len(), 1);
        // Label call was attempted (recorded).
        assert_eq!(writer.labels_removed.lock().unwrap().len(), 1);
        // Failure surfaced.
        let msg = rx.try_recv().expect("label failure should surface");
        assert!(msg.contains("label remove"), "got {msg}");
    }

    #[tokio::test]
    async fn notify_returns_immediately() {
        // Sanity: the public methods must not block on the (potentially
        // slow) writer. We give the writer a writer that sleeps for
        // 200ms; the call should return well under that.
        struct SlowWriter;
        impl JiraWriter for SlowWriter {
            fn add_comment<'a>(
                &'a self,
                _key: &'a str,
                _body: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>>
            {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(())
                })
            }

            fn remove_label<'a>(
                &'a self,
                _key: &'a str,
                _label: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>>
            {
                Box::pin(async { Ok(()) })
            }

            fn transition<'a>(
                &'a self,
                _key: &'a str,
                _id: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<(), JiraError>> + Send + 'a>>
            {
                Box::pin(async { Ok(()) })
            }
        }

        let wb = JiraWriteback::new(
            Arc::new(SlowWriter) as Arc<dyn JiraWriter>,
            JiraWritebackConfig::default(),
            None,
        );

        let start = std::time::Instant::now();
        wb.notify_exploring("PROJ-1");
        let dur = start.elapsed();
        assert!(
            dur < Duration::from_millis(50),
            "notify must return promptly; took {:?}",
            dur
        );
        // Now drain the spawned task so it doesn't outlive the runtime.
        wb.wait_idle().await;
    }

    #[tokio::test]
    async fn dropped_error_receiver_is_silent() {
        // If the daemon binary's drainer task crashed (or never wired
        // a receiver in the first place), failures must still be
        // logged and swallowed, not panic the spawned task.
        let writer = RecordingWriter::new();
        writer.fail_next_comments();
        let (wb, rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());
        drop(rx);

        wb.notify_archived("PROJ-1");
        wb.wait_idle().await;

        // We have no way to assert the warn! fired without a tracing
        // subscriber, but the in_flight counter returning to 0 is
        // sufficient evidence that the task didn't panic.
        assert_eq!(wb.pending(), 0);
    }

    #[tokio::test]
    async fn cancelled_path_also_removes_label() {
        // Cancelled is the symmetric path to Exploring start: if a
        // user rejects mid-flight, we want the trigger label gone so
        // the next poll doesn't recreate the workflow.
        let writer = RecordingWriter::new();
        let (wb, _rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_cancelled("PROJ-1");
        wb.wait_idle().await;

        assert_eq!(writer.labels_removed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn archived_path_does_not_touch_label() {
        // Archived means the workflow ran cleanly to completion. The
        // label was already removed when we entered Exploring; doing
        // it again is wasted API budget.
        let writer = RecordingWriter::new();
        let (wb, _rx) =
            writeback_with(Arc::clone(&writer), JiraWritebackConfig::default());

        wb.notify_archived("PROJ-1");
        wb.wait_idle().await;

        assert!(writer.labels_removed.lock().unwrap().is_empty());
    }

    #[test]
    fn config_default_matches_spec_defaults() {
        let c = JiraWritebackConfig::default();
        assert!(c.comments, "comments default on per spec");
        assert!(c.labels, "labels default on per spec");
        assert!(c.transitions.is_empty(), "transitions default off per spec");
        assert_eq!(c.trigger_label, DEFAULT_TRIGGER_LABEL);
    }
}
