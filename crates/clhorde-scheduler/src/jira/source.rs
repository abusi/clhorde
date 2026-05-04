//! Jira polling source.
//!
//! Section 4 of the `add-jira-source` change. The poll loop sits beside
//! the OpenSpec [`crate::watcher`] in the orchestrator's mental model:
//! both produce events on a [`tokio::sync::mpsc`] channel that the
//! daemon binary forwards into
//! [`crate::orchestrator::Orchestrator::handle_source_event`].
//!
//! ## Anatomy
//! - [`JiraSourceConfig`] / [`QueueConfig`] are the runtime-shaped view
//!   of the `[sources.jira]` block in `keymap.toml`. The TOML schema
//!   itself lands in section 8; section 4 only needs the in-memory shape
//!   so the loop can be wired up and tested.
//! - [`IssueSearch`] is the abstraction the loop calls into. The real
//!   [`crate::jira::JiraClient`] implements it; tests pass a synthetic
//!   implementation to drive `TicketAppeared` / `TicketLeftFilter` /
//!   stable / network-down without spinning up a mock HTTP server.
//! - [`JiraSourceStore`] owns the on-disk last-seen snapshot per queue.
//!   Snapshots are deliberately cheap (one tiny JSON per queue, atomic
//!   write) and idempotent on stale data — the orchestrator's
//!   `create_workflow` is a no-op when a same-source workflow already
//!   exists, so re-emitting `TicketAppeared` on restart is safe.
//! - [`JiraSource`] is the loop itself. `poll_once` is the unit-testable
//!   primitive that does one tick across every configured queue;
//!   [`spawn`] is the production entry point that drives `poll_once` on
//!   a timer.
//!
//! ## Why poll, not push?
//! See `design.md` §D4. Webhooks are a v2 concern; v1 polls.
//!
//! ## Persistence shape
//! `<data_dir>/jira/<queue_name>.json` →
//! ```json
//! { "last_seen": ["PROJ-1", "PROJ-7"] }
//! ```
//! Loadable even if it's stale: anything in the snapshot but not in the
//! current filter emits a `TicketLeftFilter` (a no-op for an
//! orchestrator that doesn't have that workflow). Anything in the
//! filter but not in the snapshot emits `TicketAppeared` (idempotent on
//! the orchestrator's same-source create check).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::client::JiraClient;
use super::error::JiraError;
use super::event::{JiraEvent, JiraTicketPayload};

/// Hard floor on the poll cadence. Any caller-supplied interval shorter
/// than this is silently raised — Jira Cloud rate-limits in the low
/// double digits per minute on small instances and we'd rather log a
/// warning at startup than get 429-banned.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Default cadence used when the config omits `poll_interval_secs`.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Default `maxResults` forwarded to Jira's `/search`. The server caps
/// at 100; keeping the request explicit avoids surprises if Atlassian
/// ever changes the implicit default.
pub const DEFAULT_MAX_RESULTS: u32 = 100;

/// Default per-source concurrent explore cap (per design D7). A
/// configured `[sources.jira] max_concurrent_explore` overrides this.
pub const DEFAULT_MAX_CONCURRENT_EXPLORE: usize = 5;

/// Runtime config for the Jira source.
///
/// Constructed from the parsed `[sources.jira]` block in section 8;
/// here we just need the strongly-typed shape so the loop can compile
/// and be tested.
#[derive(Debug, Clone)]
pub struct JiraSourceConfig {
    /// Time between polls. [`new`](Self::new) clamps to
    /// [`MIN_POLL_INTERVAL`].
    pub poll_interval: Duration,
    /// One queue per `[sources.jira.queues.<name>]` block. Order is
    /// stable across polls so the diff logic is deterministic.
    pub queues: Vec<QueueConfig>,
    /// Max issues fetched per poll per queue. Defaults to
    /// [`DEFAULT_MAX_RESULTS`].
    pub max_results: u32,
}

impl JiraSourceConfig {
    /// Build a config, clamping `poll_interval` to at least
    /// [`MIN_POLL_INTERVAL`]. Returns the clamped value alongside the
    /// config so the caller can log a warning when clamping happened.
    pub fn new(poll_interval: Duration, queues: Vec<QueueConfig>) -> Self {
        let clamped = poll_interval.max(MIN_POLL_INTERVAL);
        Self {
            poll_interval: clamped,
            queues,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// True when [`Self::new`] raised the configured value to
    /// [`MIN_POLL_INTERVAL`]. Used by the daemon to log a one-shot
    /// warning at startup.
    pub fn was_clamped(requested: Duration) -> bool {
        requested < MIN_POLL_INTERVAL
    }
}

impl Default for JiraSourceConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            queues: Vec::new(),
            max_results: DEFAULT_MAX_RESULTS,
        }
    }
}

/// One named JQL queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Symbolic name (e.g. `"backlog"`). Used as the persistence key
    /// and in log messages — must be a valid filename.
    pub name: String,
    /// Raw JQL forwarded to Jira. The source does not parse or rewrite
    /// it; bad JQL surfaces as a `Client { 400, .. }` from the REST
    /// client and lands in the source-health surface.
    pub filter_jql: String,
}

/// What the loop does with a search result. Decoupled from the real
/// `mpsc::Sender` so [`JiraSource::poll_once`] is a pure function in
/// tests — drive it with a synthetic searcher, get a list of outcomes
/// back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// An event the loop wants forwarded to the orchestrator.
    Event(JiraEvent),
    /// The search call failed for `queue`. Carried so the daemon can
    /// stamp the source-health surface with the message.
    Error { queue: String, message: String },
}

/// Abstraction over the two operations the source needs from a Jira
/// client: list the issues currently matching a JQL filter. Real
/// implementation is [`JiraClient`]; tests provide a synthetic version
/// that returns canned responses.
pub trait IssueSearch: Send + Sync + 'static {
    /// Run a JQL search and return the matching tickets projected into
    /// [`JiraTicketPayload`]. The source caps `max_results` per the
    /// config. The future is boxed because traits with `async fn`
    /// require it for trait objects, and the source uses `Arc<dyn
    /// IssueSearch>` so the spawn helper can keep the trait object
    /// behind a single allocation.
    fn search<'a>(
        &'a self,
        jql: &'a str,
        max_results: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<JiraTicketPayload>, JiraError>> + Send + 'a>>;
}

impl IssueSearch for JiraClient {
    fn search<'a>(
        &'a self,
        jql: &'a str,
        max_results: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<JiraTicketPayload>, JiraError>> + Send + 'a>> {
        Box::pin(async move { self.search_jql(jql, max_results).await })
    }
}

/// Per-queue persistence: one tiny JSON file per queue. Snapshots are
/// recoverable from stale data — see the module-level docs.
#[derive(Debug, Clone)]
pub struct JiraSourceStore {
    dir: PathBuf,
}

impl JiraSourceStore {
    /// Open a store rooted at `dir`. The directory is created lazily
    /// on the first write — `open` itself never touches the
    /// filesystem.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Default location: `<data_dir>/jira/`.
    pub fn open_default() -> Option<Self> {
        clhorde_core::config::data_dir().map(|d| Self::open(d.join("jira")))
    }

    fn file_path(&self, queue_name: &str) -> PathBuf {
        // Queue names land in the path verbatim. The config layer
        // (section 8) is responsible for refusing names with `/`,
        // `\`, NUL, or `..`; here we just use them.
        self.dir.join(format!("{queue_name}.json"))
    }

    /// Load the last-seen set for one queue. Returns an empty set on
    /// any error (missing file, malformed JSON, IO failure) — the
    /// snapshot is an optimisation, not a source of truth, so a bad
    /// snapshot must never block the loop from starting.
    pub fn load(&self, queue_name: &str) -> BTreeSet<String> {
        let path = self.file_path(queue_name);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return BTreeSet::new(),
        };
        match serde_json::from_str::<PersistedQueueState>(&raw) {
            Ok(state) => state.last_seen.into_iter().collect(),
            Err(_) => BTreeSet::new(),
        }
    }

    /// Atomically persist `last_seen` for `queue_name`. Logs and
    /// swallows IO errors — write failures must not crash the loop;
    /// the next poll will re-emit any events that lost their snapshot
    /// and that's idempotent for the orchestrator.
    pub fn save(&self, queue_name: &str, last_seen: &BTreeSet<String>) {
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(
                queue = %queue_name,
                error = %e,
                "could not create jira source state dir"
            );
            return;
        }
        let payload = PersistedQueueState {
            last_seen: last_seen.iter().cloned().collect(),
        };
        let serialised = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(queue = %queue_name, error = %e, "could not serialise jira state");
                return;
            }
        };
        let final_path = self.file_path(queue_name);
        let tmp_path = final_path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp_path, serialised.as_bytes()) {
            tracing::warn!(
                queue = %queue_name,
                path = %tmp_path.display(),
                error = %e,
                "could not write jira state tmp file"
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
            tracing::warn!(
                queue = %queue_name,
                error = %e,
                "could not rename jira state tmp file"
            );
            // Best-effort cleanup; ignore failure.
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedQueueState {
    #[serde(default)]
    last_seen: Vec<String>,
}

/// Per-queue in-memory diff state, indexed by queue name.
#[derive(Debug, Default)]
struct QueueState {
    last_seen: BTreeSet<String>,
}

/// The poll loop, as a struct with a `poll_once` method tests can
/// drive deterministically. Production users go through [`spawn`].
pub struct JiraSource {
    client: Arc<dyn IssueSearch>,
    config: JiraSourceConfig,
    queues: HashMap<String, QueueState>,
    store: Option<JiraSourceStore>,
}

impl JiraSource {
    /// Build a source. The persistence store is optional so tests can
    /// skip the filesystem entirely; production daemons pass
    /// `Some(JiraSourceStore::open_default())`.
    pub fn new(
        client: Arc<dyn IssueSearch>,
        config: JiraSourceConfig,
        store: Option<JiraSourceStore>,
    ) -> Self {
        let mut queues = HashMap::with_capacity(config.queues.len());
        for q in &config.queues {
            let last_seen = store
                .as_ref()
                .map(|s| s.load(&q.name))
                .unwrap_or_default();
            queues.insert(q.name.clone(), QueueState { last_seen });
        }
        Self {
            client,
            config,
            queues,
            store,
        }
    }

    /// One full pass across every configured queue. Returns the
    /// outcomes (events to forward + per-queue errors) without sending
    /// them anywhere — callers wire the outcomes to whatever sink
    /// makes sense ([`spawn`] forwards events through an
    /// `mpsc::UnboundedSender`).
    ///
    /// The diff is computed against the in-memory `last_seen` set;
    /// after a successful poll, that set is updated and persisted. A
    /// failed poll leaves both untouched, so on recovery the next
    /// poll re-diffs against the same baseline (no spurious
    /// `TicketLeftFilter` storm during a Jira outage).
    pub async fn poll_once(&mut self) -> Vec<PollOutcome> {
        let mut outcomes = Vec::new();
        // Iterate in config order so test assertions are stable.
        for queue in self.config.queues.clone() {
            let result = self
                .client
                .search(&queue.filter_jql, self.config.max_results)
                .await;
            match result {
                Ok(issues) => {
                    let outcomes_for_queue = self.diff_and_update(&queue.name, issues);
                    outcomes.extend(outcomes_for_queue);
                }
                Err(e) => {
                    outcomes.push(PollOutcome::Error {
                        queue: queue.name.clone(),
                        message: e.to_string(),
                    });
                }
            }
        }
        outcomes
    }

    fn diff_and_update(
        &mut self,
        queue_name: &str,
        issues: Vec<JiraTicketPayload>,
    ) -> Vec<PollOutcome> {
        // Index payloads by key so emitted events carry the full
        // payload and the diff key set is cheap.
        let mut current_by_key: HashMap<String, JiraTicketPayload> = HashMap::new();
        let mut current_keys: BTreeSet<String> = BTreeSet::new();
        for p in issues {
            // Defensive: drop entries with empty keys. Jira shouldn't
            // ever return them; if it does we'd otherwise emit
            // garbage.
            if p.key.is_empty() {
                continue;
            }
            current_keys.insert(p.key.clone());
            current_by_key.insert(p.key.clone(), p);
        }

        let state = self
            .queues
            .entry(queue_name.to_string())
            .or_insert_with(QueueState::default);

        let appeared: Vec<String> = current_keys
            .difference(&state.last_seen)
            .cloned()
            .collect();
        let left: Vec<String> = state
            .last_seen
            .difference(&current_keys)
            .cloned()
            .collect();

        let mut outcomes = Vec::with_capacity(appeared.len() + left.len());
        for key in appeared {
            // Unwrap safe: every key in `current_keys` was inserted
            // alongside its payload above.
            let payload = current_by_key.remove(&key).unwrap_or_else(|| {
                // Fallback shape so a logic bug here can't take the
                // loop down: emit an event with just the key set.
                let mut p = JiraTicketPayload::default();
                p.key = key.clone();
                p
            });
            outcomes.push(PollOutcome::Event(JiraEvent::TicketAppeared {
                key,
                payload,
            }));
        }
        for key in left {
            outcomes.push(PollOutcome::Event(JiraEvent::TicketLeftFilter { key }));
        }

        state.last_seen = current_keys;
        if let Some(store) = &self.store {
            store.save(queue_name, &state.last_seen);
        }
        outcomes
    }

    /// Test introspection: return the current in-memory last-seen set
    /// for `queue_name`, or an empty set if the queue is unknown.
    #[cfg(test)]
    fn last_seen(&self, queue_name: &str) -> BTreeSet<String> {
        self.queues
            .get(queue_name)
            .map(|s| s.last_seen.clone())
            .unwrap_or_default()
    }
}

/// Handle returned by [`spawn`]. Dropping it cancels the loop on the
/// next iteration boundary. The daemon keeps it alive until shutdown.
pub struct JiraSourceHandle {
    join: JoinHandle<()>,
}

impl JiraSourceHandle {
    /// Abort the poll loop and wait for it to wind down. Useful in
    /// tests where the spawned task would otherwise outlive the
    /// runtime; production code drops the handle on daemon shutdown.
    pub async fn shutdown(self) {
        self.join.abort();
        let _ = self.join.await;
    }
}

/// Spawn the long-lived poll loop. Each tick fans events out through
/// `tx` and per-queue errors through the channel observable on the
/// returned handle is implicit (failures land in `tracing::warn!` and
/// the next iteration retries).
///
/// `tx` is the orchestrator's Jira-event channel — the daemon binary
/// wraps every received event in [`crate::source::SourceEvent::Jira`]
/// before calling [`crate::orchestrator::Orchestrator::handle_source_event`].
///
/// On a search failure, this helper falls back to logging a warning
/// rather than crashing the loop. Source-health bookkeeping happens
/// orchestrator-side via
/// [`crate::orchestrator::Orchestrator::record_source_error`]; the
/// caller is responsible for forwarding the per-queue messages on the
/// `error_tx` channel if it cares about them. Most call sites only
/// need event-level signalling, hence the simple shape.
pub fn spawn(
    mut source: JiraSource,
    tx: mpsc::UnboundedSender<JiraEvent>,
    error_tx: Option<mpsc::UnboundedSender<(String, String)>>,
) -> JiraSourceHandle {
    let join = tokio::spawn(async move {
        loop {
            let outcomes = source.poll_once().await;
            for outcome in outcomes {
                match outcome {
                    PollOutcome::Event(ev) => {
                        if tx.send(ev).is_err() {
                            // Receiver dropped — orchestrator is gone,
                            // no point continuing.
                            return;
                        }
                    }
                    PollOutcome::Error { queue, message } => {
                        tracing::warn!(
                            queue = %queue,
                            error = %message,
                            "jira poll failed; will retry on next tick"
                        );
                        if let Some(etx) = &error_tx {
                            let _ = etx.send((queue, message));
                        }
                    }
                }
            }
            tokio::time::sleep(source.config.poll_interval).await;
        }
    });
    JiraSourceHandle { join }
}

/// Per-source explore-concurrency gate (per design D7).
///
/// The Jira poll loop emits a `TicketAppeared` event for every new
/// issue matching its filters. If the orchestrator already has
/// `cap` explore workers alive for this source, additional events
/// would either dispatch over budget (clobbering `max_workers`
/// fairness) or get dropped silently. The gate sits between the
/// poll loop and the orchestrator: it admits up to `cap` events
/// directly, and parks the rest in a small in-memory queue. When a
/// previously-admitted workflow leaves `Exploring` (approve, reject,
/// ticket-left-filter, reaper kill), the gate releases the slot and
/// flushes one queued event.
///
/// Wiring is the daemon binary's responsibility — this type is
/// pure data and exposes the operations:
/// - [`Self::admit`] — the poll loop calls this on every
///   `TicketAppeared` payload; the gate either returns the event
///   for forwarding or holds it.
/// - [`Self::release`] — the orchestrator-side observer calls this
///   when a workflow leaves `Exploring`; the gate returns the next
///   queued event, if any.
///
/// The gate stores payloads (not events) internally because every
/// queued ticket is by construction a `TicketAppeared`; the variant
/// is reconstructed at flush time. Order is FIFO via `VecDeque` so
/// a Jira queue with strict priority semantics doesn't get
/// reordered by gating.
#[derive(Debug)]
pub struct ConcurrencyGate {
    cap: usize,
    /// Currently-admitted workflow keys. The gate doesn't care
    /// about phase — it just tracks slots until [`Self::release`]
    /// is called.
    active: HashSet<String>,
    /// FIFO of payloads waiting on a slot.
    queue: VecDeque<JiraTicketPayload>,
}

impl ConcurrencyGate {
    /// Build a gate with capacity `cap`. A `cap` of zero degenerates
    /// to "queue everything forever" — useful as a kill-switch but
    /// otherwise a misconfiguration; the config layer (section 8)
    /// rejects zero at startup.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            active: HashSet::new(),
            queue: VecDeque::new(),
        }
    }

    /// Configured capacity. Exposed for status reporting.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Number of slots currently in use.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Number of payloads waiting in the in-memory queue.
    pub fn queued_count(&self) -> usize {
        self.queue.len()
    }

    /// Admit a payload. If a slot is free (`active_count < cap`),
    /// returns `Some(JiraEvent::TicketAppeared{..})` for the caller
    /// to forward; the workflow key is recorded in `active`.
    /// Otherwise the payload is queued and `None` is returned.
    ///
    /// Re-admission of an already-active key is a no-op (returns
    /// `None`) and does not enqueue: the polling loop is allowed to
    /// re-emit `TicketAppeared` for tickets it already saw, and the
    /// gate must not double-count those.
    pub fn admit(&mut self, payload: JiraTicketPayload) -> Option<JiraEvent> {
        if self.active.contains(&payload.key) {
            return None;
        }
        if self.active.len() < self.cap {
            self.active.insert(payload.key.clone());
            return Some(JiraEvent::TicketAppeared {
                key: payload.key.clone(),
                payload,
            });
        }
        // Cap reached: queue. Don't dedupe queued payloads — the
        // first admission "owns" the slot, but a re-emission while
        // queued is a no-op so we don't accumulate stale copies.
        if self.queue.iter().any(|p| p.key == payload.key) {
            return None;
        }
        self.queue.push_back(payload);
        None
    }

    /// Release the slot held by `key`. Returns the next queued
    /// event if one is available; the freed slot is immediately
    /// re-occupied by that event's key (the caller forwards it on
    /// the source-event channel).
    ///
    /// No-op if `key` isn't currently active. Releases that beat
    /// admits (e.g. an out-of-order `TicketLeftFilter` for a key we
    /// never sent through `admit`) silently drop the queued head
    /// is left alone.
    pub fn release(&mut self, key: &str) -> Option<JiraEvent> {
        if !self.active.remove(key) {
            return None;
        }
        let next = self.queue.pop_front()?;
        self.active.insert(next.key.clone());
        Some(JiraEvent::TicketAppeared {
            key: next.key.clone(),
            payload: next,
        })
    }

    /// Drop a queued payload without releasing a slot. Used when a
    /// `TicketLeftFilter` arrives for a key that hasn't been
    /// admitted yet (still in the queue) — the workflow shouldn't
    /// fire when its slot eventually frees.
    pub fn drop_queued(&mut self, key: &str) -> bool {
        let before = self.queue.len();
        self.queue.retain(|p| p.key != key);
        self.queue.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn payload(key: &str, title: &str) -> JiraTicketPayload {
        JiraTicketPayload {
            key: key.to_string(),
            title: title.to_string(),
            description: String::new(),
            acceptance_criteria: String::new(),
            labels: Vec::new(),
            reporter: None,
        }
    }

    /// Drives the source loop deterministically: each `search` call
    /// pops the next response from a list. Lets tests script
    /// "ticket appears", "ticket gone", "network down" in sequence.
    struct ScriptedSearch {
        responses: Mutex<Vec<Result<Vec<JiraTicketPayload>, JiraError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedSearch {
        fn new(responses: Vec<Result<Vec<JiraTicketPayload>, JiraError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl IssueSearch for ScriptedSearch {
        fn search<'a>(
            &'a self,
            jql: &'a str,
            _max_results: u32,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<JiraTicketPayload>, JiraError>>
                    + Send
                    + 'a,
            >,
        > {
            self.calls.lock().unwrap().push(jql.to_string());
            let next = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Ok(Vec::new()));
            Box::pin(async move { next })
        }
    }

    fn one_queue(jql: &str) -> JiraSourceConfig {
        let mut cfg = JiraSourceConfig::new(
            DEFAULT_POLL_INTERVAL,
            vec![QueueConfig {
                name: "backlog".to_string(),
                filter_jql: jql.to_string(),
            }],
        );
        cfg.max_results = 50;
        cfg
    }

    #[test]
    fn poll_interval_is_clamped_to_floor() {
        let cfg = JiraSourceConfig::new(Duration::from_secs(1), vec![]);
        assert_eq!(cfg.poll_interval, MIN_POLL_INTERVAL);
        assert!(JiraSourceConfig::was_clamped(Duration::from_secs(1)));
    }

    #[test]
    fn poll_interval_above_floor_is_kept() {
        let cfg = JiraSourceConfig::new(Duration::from_secs(60), vec![]);
        assert_eq!(cfg.poll_interval, Duration::from_secs(60));
        assert!(!JiraSourceConfig::was_clamped(Duration::from_secs(60)));
    }

    #[test]
    fn floor_value_is_kept() {
        let cfg = JiraSourceConfig::new(MIN_POLL_INTERVAL, vec![]);
        assert_eq!(cfg.poll_interval, MIN_POLL_INTERVAL);
        assert!(!JiraSourceConfig::was_clamped(MIN_POLL_INTERVAL));
    }

    #[tokio::test]
    async fn appeared_emits_ticket_appeared_for_each_new_key() {
        // Responses are popped from the back, so script in reverse order:
        // first poll returns [PROJ-1, PROJ-2].
        let scripted = ScriptedSearch::new(vec![Ok(vec![
            payload("PROJ-1", "First"),
            payload("PROJ-2", "Second"),
        ])]);
        let mut source =
            JiraSource::new(Arc::new(scripted), one_queue("project = PROJ"), None);

        let outcomes = source.poll_once().await;
        let events: Vec<&JiraEvent> = outcomes
            .iter()
            .filter_map(|o| match o {
                PollOutcome::Event(ev) => Some(ev),
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 2);
        // Both events are TicketAppeared, key set is {PROJ-1, PROJ-2}.
        let keys: BTreeSet<String> = events
            .iter()
            .map(|ev| match ev {
                JiraEvent::TicketAppeared { key, .. } => key.clone(),
                _ => panic!("unexpected event variant: {ev:?}"),
            })
            .collect();
        assert!(keys.contains("PROJ-1"));
        assert!(keys.contains("PROJ-2"));
        assert_eq!(
            source.last_seen("backlog"),
            BTreeSet::from(["PROJ-1".to_string(), "PROJ-2".to_string()])
        );
    }

    #[tokio::test]
    async fn stable_set_emits_no_events_on_second_poll() {
        let scripted = ScriptedSearch::new(vec![
            Ok(vec![payload("PROJ-1", "First")]),
            Ok(vec![payload("PROJ-1", "First")]),
        ]);
        let mut source =
            JiraSource::new(Arc::new(scripted), one_queue("project = PROJ"), None);

        let first = source.poll_once().await;
        assert_eq!(first.len(), 1);
        let second = source.poll_once().await;
        assert!(
            second.is_empty(),
            "stable set must not emit events; got {second:?}"
        );
    }

    #[tokio::test]
    async fn ticket_leaving_filter_emits_left_event() {
        let scripted = ScriptedSearch::new(vec![
            // Second poll: PROJ-1 only (PROJ-2 left the filter).
            Ok(vec![payload("PROJ-1", "First")]),
            // First poll: both present.
            Ok(vec![payload("PROJ-1", "First"), payload("PROJ-2", "Second")]),
        ]);
        let mut source =
            JiraSource::new(Arc::new(scripted), one_queue("project = PROJ"), None);

        let first = source.poll_once().await;
        assert_eq!(first.len(), 2);
        let second = source.poll_once().await;
        assert_eq!(second.len(), 1);
        match &second[0] {
            PollOutcome::Event(JiraEvent::TicketLeftFilter { key }) => {
                assert_eq!(key, "PROJ-2");
            }
            other => panic!("expected TicketLeftFilter, got {other:?}"),
        }
        // Snapshot is now {PROJ-1} only.
        assert_eq!(
            source.last_seen("backlog"),
            BTreeSet::from(["PROJ-1".to_string()])
        );
    }

    #[tokio::test]
    async fn appeared_and_left_emitted_in_one_poll() {
        let scripted = ScriptedSearch::new(vec![
            // Second poll: PROJ-2 + PROJ-3.
            Ok(vec![payload("PROJ-2", "Second"), payload("PROJ-3", "Third")]),
            // First poll: PROJ-1 + PROJ-2.
            Ok(vec![payload("PROJ-1", "First"), payload("PROJ-2", "Second")]),
        ]);
        let mut source =
            JiraSource::new(Arc::new(scripted), one_queue("project = PROJ"), None);

        // Seed.
        let _ = source.poll_once().await;
        let outcomes = source.poll_once().await;
        let mut appeared = Vec::new();
        let mut left = Vec::new();
        for o in &outcomes {
            match o {
                PollOutcome::Event(JiraEvent::TicketAppeared { key, .. }) => {
                    appeared.push(key.clone());
                }
                PollOutcome::Event(JiraEvent::TicketLeftFilter { key }) => {
                    left.push(key.clone());
                }
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        assert_eq!(appeared, vec!["PROJ-3".to_string()]);
        assert_eq!(left, vec!["PROJ-1".to_string()]);
    }

    #[tokio::test]
    async fn network_failure_records_error_and_does_not_lose_state() {
        let scripted = ScriptedSearch::new(vec![
            // Third poll: PROJ-1 still there.
            Ok(vec![payload("PROJ-1", "First")]),
            // Second poll: network failure.
            Err(JiraError::Network("connection refused".to_string())),
            // First poll: PROJ-1.
            Ok(vec![payload("PROJ-1", "First")]),
        ]);
        let mut source =
            JiraSource::new(Arc::new(scripted), one_queue("project = PROJ"), None);

        let first = source.poll_once().await;
        assert_eq!(first.len(), 1, "first poll seeds with PROJ-1");

        let second = source.poll_once().await;
        assert_eq!(second.len(), 1);
        match &second[0] {
            PollOutcome::Error { queue, message } => {
                assert_eq!(queue, "backlog");
                assert!(message.contains("network"), "got: {message}");
            }
            other => panic!("expected Error outcome, got {other:?}"),
        }
        // State preserved across the failure.
        assert_eq!(
            source.last_seen("backlog"),
            BTreeSet::from(["PROJ-1".to_string()])
        );

        let third = source.poll_once().await;
        assert!(
            third.is_empty(),
            "post-recovery poll on stable set must be empty; got {third:?}"
        );
    }

    #[tokio::test]
    async fn multiple_queues_are_independent() {
        let scripted = ScriptedSearch::new(vec![
            // Second queue (`urgent`): PROJ-9 only.
            Ok(vec![payload("PROJ-9", "Urgent")]),
            // First queue (`backlog`): PROJ-1.
            Ok(vec![payload("PROJ-1", "First")]),
        ]);
        let cfg = JiraSourceConfig::new(
            DEFAULT_POLL_INTERVAL,
            vec![
                QueueConfig {
                    name: "backlog".to_string(),
                    filter_jql: "labels = clhorde-plan".to_string(),
                },
                QueueConfig {
                    name: "urgent".to_string(),
                    filter_jql: "priority = Highest".to_string(),
                },
            ],
        );
        let mut source = JiraSource::new(Arc::new(scripted), cfg, None);

        let outcomes = source.poll_once().await;
        // Two TicketAppeared events, one per queue.
        let keys: Vec<String> = outcomes
            .iter()
            .filter_map(|o| match o {
                PollOutcome::Event(JiraEvent::TicketAppeared { key, .. }) => {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"PROJ-1".to_string()));
        assert!(keys.contains(&"PROJ-9".to_string()));

        assert_eq!(
            source.last_seen("backlog"),
            BTreeSet::from(["PROJ-1".to_string()])
        );
        assert_eq!(
            source.last_seen("urgent"),
            BTreeSet::from(["PROJ-9".to_string()])
        );
    }

    #[test]
    fn store_round_trips_last_seen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = JiraSourceStore::open(tmp.path());
        let mut set = BTreeSet::new();
        set.insert("PROJ-1".to_string());
        set.insert("PROJ-2".to_string());
        store.save("backlog", &set);

        let loaded = store.load("backlog");
        assert_eq!(loaded, set);
    }

    #[test]
    fn store_load_returns_empty_for_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = JiraSourceStore::open(tmp.path());
        let loaded = store.load("never-written");
        assert!(loaded.is_empty());
    }

    #[test]
    fn store_load_tolerates_garbled_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = JiraSourceStore::open(tmp.path());
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("backlog.json"), "not json at all").unwrap();
        // Stale/broken snapshot must not panic — just degrade to
        // empty so the next poll re-emits TicketAppeared.
        let loaded = store.load("backlog");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn store_persists_across_source_recreation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = JiraSourceStore::open(tmp.path());

        // First incarnation: see PROJ-1.
        {
            let scripted = ScriptedSearch::new(vec![Ok(vec![payload("PROJ-1", "First")])]);
            let mut source = JiraSource::new(
                Arc::new(scripted),
                one_queue("project = PROJ"),
                Some(store.clone()),
            );
            let _ = source.poll_once().await;
        }

        // Second incarnation: same set on disk, same response →
        // no events fired. This is the "restart doesn't re-comment
        // on every existing ticket" property from task 4.4.
        {
            let scripted = ScriptedSearch::new(vec![Ok(vec![payload("PROJ-1", "First")])]);
            let mut source = JiraSource::new(
                Arc::new(scripted),
                one_queue("project = PROJ"),
                Some(store.clone()),
            );
            let outcomes = source.poll_once().await;
            assert!(
                outcomes.is_empty(),
                "restart with stable filter must emit nothing; got {outcomes:?}"
            );
        }
    }

    #[tokio::test]
    async fn stale_snapshot_emits_left_for_now_absent_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = JiraSourceStore::open(tmp.path());
        // Pre-populate snapshot with a ticket that's no longer in the filter.
        let mut stale = BTreeSet::new();
        stale.insert("PROJ-OLD".to_string());
        store.save("backlog", &stale);

        let scripted = ScriptedSearch::new(vec![Ok(vec![payload("PROJ-1", "First")])]);
        let mut source = JiraSource::new(
            Arc::new(scripted),
            one_queue("project = PROJ"),
            Some(store.clone()),
        );

        let outcomes = source.poll_once().await;
        let mut appeared = Vec::new();
        let mut left = Vec::new();
        for o in outcomes {
            match o {
                PollOutcome::Event(JiraEvent::TicketAppeared { key, .. }) => appeared.push(key),
                PollOutcome::Event(JiraEvent::TicketLeftFilter { key }) => left.push(key),
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        assert_eq!(appeared, vec!["PROJ-1".to_string()]);
        assert_eq!(left, vec!["PROJ-OLD".to_string()]);
    }

    #[tokio::test]
    async fn dropped_payload_with_empty_key_is_ignored() {
        let scripted = ScriptedSearch::new(vec![Ok(vec![
            payload("", "Bogus"),
            payload("PROJ-1", "First"),
        ])]);
        let mut source =
            JiraSource::new(Arc::new(scripted), one_queue("project = PROJ"), None);
        let outcomes = source.poll_once().await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            source.last_seen("backlog"),
            BTreeSet::from(["PROJ-1".to_string()])
        );
    }

    #[tokio::test]
    async fn spawn_forwards_events_until_channel_closes() {
        // Drive the loop through one tick by giving it one canned
        // response. The poll interval doesn't matter for this test —
        // we drop the receiver and let the loop notice.
        let scripted = ScriptedSearch::new(vec![Ok(vec![payload("PROJ-1", "First")])]);
        let mut cfg = one_queue("project = PROJ");
        // Tightest we can use; the loop sleeps after the first send
        // so wall-time impact in this test is bounded by how fast we
        // recv and drop.
        cfg.poll_interval = MIN_POLL_INTERVAL;
        let source = JiraSource::new(Arc::new(scripted), cfg, None);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = spawn(source, tx, None);

        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("poll loop must emit at least one event quickly")
            .expect("channel must still be open");
        match ev {
            JiraEvent::TicketAppeared { key, .. } => assert_eq!(key, "PROJ-1"),
            other => panic!("expected TicketAppeared, got {other:?}"),
        }
        drop(rx);
        handle.shutdown().await;
    }

    // ── ConcurrencyGate ──

    fn appeared_key(ev: &JiraEvent) -> &str {
        match ev {
            JiraEvent::TicketAppeared { key, .. } => key,
            other => panic!("expected TicketAppeared, got {other:?}"),
        }
    }

    #[test]
    fn gate_admits_up_to_capacity_directly() {
        let mut gate = ConcurrencyGate::new(2);
        let a = gate.admit(payload("PROJ-1", "First"));
        let b = gate.admit(payload("PROJ-2", "Second"));
        assert!(a.is_some());
        assert!(b.is_some());
        assert_eq!(appeared_key(a.as_ref().unwrap()), "PROJ-1");
        assert_eq!(appeared_key(b.as_ref().unwrap()), "PROJ-2");
        assert_eq!(gate.active_count(), 2);
        assert_eq!(gate.queued_count(), 0);
    }

    #[test]
    fn gate_queues_overflow() {
        let mut gate = ConcurrencyGate::new(1);
        let a = gate.admit(payload("PROJ-1", "First"));
        let b = gate.admit(payload("PROJ-2", "Second"));
        assert!(a.is_some());
        assert!(b.is_none(), "second admit should queue");
        assert_eq!(gate.active_count(), 1);
        assert_eq!(gate.queued_count(), 1);
    }

    #[test]
    fn gate_release_flushes_one_queued() {
        let mut gate = ConcurrencyGate::new(1);
        gate.admit(payload("PROJ-1", "First"));
        gate.admit(payload("PROJ-2", "Second"));
        gate.admit(payload("PROJ-3", "Third"));
        assert_eq!(gate.queued_count(), 2);

        let next = gate.release("PROJ-1").expect("release should flush PROJ-2");
        assert_eq!(appeared_key(&next), "PROJ-2");
        assert_eq!(gate.active_count(), 1);
        assert_eq!(gate.queued_count(), 1);

        let next = gate.release("PROJ-2").expect("release should flush PROJ-3");
        assert_eq!(appeared_key(&next), "PROJ-3");
        assert_eq!(gate.active_count(), 1);
        assert_eq!(gate.queued_count(), 0);
    }

    #[test]
    fn gate_release_returns_none_when_queue_empty() {
        let mut gate = ConcurrencyGate::new(2);
        gate.admit(payload("PROJ-1", "First"));
        let next = gate.release("PROJ-1");
        assert!(next.is_none());
        assert_eq!(gate.active_count(), 0);
    }

    #[test]
    fn gate_release_unknown_key_is_noop() {
        let mut gate = ConcurrencyGate::new(2);
        gate.admit(payload("PROJ-1", "First"));
        let next = gate.release("NEVER-ADMITTED");
        assert!(next.is_none());
        assert_eq!(gate.active_count(), 1);
    }

    #[test]
    fn gate_double_admit_is_noop_for_active_key() {
        let mut gate = ConcurrencyGate::new(2);
        let first = gate.admit(payload("PROJ-1", "First"));
        let second = gate.admit(payload("PROJ-1", "First-rerun"));
        assert!(first.is_some());
        assert!(second.is_none());
        assert_eq!(gate.active_count(), 1);
        assert_eq!(gate.queued_count(), 0);
    }

    #[test]
    fn gate_double_admit_does_not_double_queue() {
        let mut gate = ConcurrencyGate::new(1);
        gate.admit(payload("PROJ-1", "First"));
        gate.admit(payload("PROJ-2", "Second"));
        gate.admit(payload("PROJ-2", "Second-rerun"));
        assert_eq!(gate.queued_count(), 1);
    }

    #[test]
    fn gate_drop_queued_removes_pending_payload() {
        let mut gate = ConcurrencyGate::new(1);
        gate.admit(payload("PROJ-1", "First"));
        gate.admit(payload("PROJ-2", "Second"));
        assert_eq!(gate.queued_count(), 1);
        assert!(gate.drop_queued("PROJ-2"));
        assert_eq!(gate.queued_count(), 0);
    }

    #[test]
    fn gate_drop_queued_unknown_returns_false() {
        let mut gate = ConcurrencyGate::new(1);
        gate.admit(payload("PROJ-1", "First"));
        assert!(!gate.drop_queued("NEVER-QUEUED"));
    }

    #[test]
    fn gate_zero_capacity_queues_everything() {
        let mut gate = ConcurrencyGate::new(0);
        let r = gate.admit(payload("PROJ-1", "First"));
        assert!(r.is_none());
        assert_eq!(gate.active_count(), 0);
        assert_eq!(gate.queued_count(), 1);
    }

    #[test]
    fn gate_release_flushes_in_fifo_order() {
        let mut gate = ConcurrencyGate::new(1);
        gate.admit(payload("PROJ-1", "First"));
        gate.admit(payload("PROJ-2", "Second"));
        gate.admit(payload("PROJ-3", "Third"));
        gate.admit(payload("PROJ-4", "Fourth"));

        let n2 = gate.release("PROJ-1").unwrap();
        let n3 = gate.release("PROJ-2").unwrap();
        let n4 = gate.release("PROJ-3").unwrap();
        assert_eq!(appeared_key(&n2), "PROJ-2");
        assert_eq!(appeared_key(&n3), "PROJ-3");
        assert_eq!(appeared_key(&n4), "PROJ-4");
    }
}
