//! Async client that bridges HTTP/WebSocket to the scheduler control socket.
//!
//! Mirrors [`crate::bridge::DaemonBridge`] but for `clhorde-scheduler`'s
//! lighter wire protocol:
//!
//! 1. A long-lived `Subscribe` connection feeds summary
//!    [`SchedulerEvent`]s (`Snapshot` / `WorkflowUpdated`) onto a
//!    `tokio::sync::broadcast` channel that every WebSocket client
//!    subscribes to. Push-based, no polling.
//! 2. A parallel long-lived `SubscribeAllDetails` connection feeds
//!    detail [`SchedulerEvent`]s (`DetailUpdated`) onto the **same**
//!    broadcast channel, so a WS client subscribed to one sink sees
//!    both kinds. The orchestrator runs the two surfaces on separate
//!    `broadcast::Sender`s — the bridge fans them in here.
//! 3. One-shot helpers (`request`) open a fresh connection per call
//!    for status/detail/queue/cancel/retry — the scheduler's own
//!    control protocol is request/response on a non-Subscribe
//!    connection, so each REST handler that needs a response just
//!    plumbs through here.
//!
//! Critically, this bridge **does not block startup**. The scheduler
//! is an optional background process — `clhorde-web` happily serves
//! the daemon-only views (Prompts) when the scheduler is offline. The
//! subscribe loops reconnect in the background and routes that need
//! the scheduler return 503 until it comes up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use clhorde_core::control::{ControlRequest, ControlResponse, SchedulerEvent};
use clhorde_core::ipc::{self, MAX_FRAME_SIZE};

/// Broadcast capacity for `SchedulerEvent`s relayed from the scheduler
/// to every connected WS client. Sized to match the scheduler-side
/// channel; if the bridge ever lags, the scheduler re-emits a fresh
/// `Snapshot` so each WS client converges back to consistent state.
const EVENT_BROADCAST_SIZE: usize = 256;

/// Initial reconnection backoff for the long-lived Subscribe loop.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Maximum reconnection backoff. Big enough that a stopped scheduler
/// doesn't get hammered, small enough that the dashboard catches up
/// within a few seconds of `clhorde-scheduler daemon` coming back.
const MAX_BACKOFF: Duration = Duration::from_secs(15);

/// Per-request timeout for one-shot calls. Mirrors the TUI's
/// `scheduler_client::REQUEST_TIMEOUT` so a stuck scheduler doesn't
/// hold an HTTP handler open indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(800);

/// Errors surfaced from the bridge to HTTP/WS handlers.
#[derive(Debug, Clone)]
pub enum SchedulerBridgeError {
    /// The scheduler control socket can't be reached. The scheduler
    /// is probably not running.
    Unreachable,
    /// I/O error after a connection was established.
    Io(String),
    /// Server replied with a malformed frame (or a frame we don't
    /// know how to interpret on this code path).
    BadResponse(String),
    /// One-shot call exceeded [`REQUEST_TIMEOUT`].
    Timeout,
}

impl std::fmt::Display for SchedulerBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerBridgeError::Unreachable => f.write_str("scheduler not reachable"),
            SchedulerBridgeError::Io(e) => write!(f, "io: {e}"),
            SchedulerBridgeError::BadResponse(s) => write!(f, "bad response: {s}"),
            SchedulerBridgeError::Timeout => f.write_str("scheduler did not respond in time"),
        }
    }
}

impl std::error::Error for SchedulerBridgeError {}

/// Long-lived bridge to `clhorde-scheduler`'s control socket.
///
/// Construction is non-blocking: [`SchedulerBridge::start`] returns
/// immediately with a fully-functional handle, and the background
/// subscribe loop reconnects independently. Use [`is_connected`] to
/// drive UI hints; use [`subscribe_events`] for push-based event
/// fan-out; use [`request`] for one-shot RPCs.
pub struct SchedulerBridge {
    socket_path: PathBuf,
    connected: AtomicBool,
    event_tx: broadcast::Sender<SchedulerEvent>,
    /// Held to keep the summary subscribe loop alive for the bridge
    /// lifetime. Wrapped in a `tokio::sync::Mutex` so the constructor
    /// can swap in the real handle after the bridge `Arc` is built
    /// (mirrors the DaemonBridge bootstrap pattern).
    _conn_handle: Mutex<tokio::task::JoinHandle<()>>,
    /// Held to keep the parallel detail subscribe loop alive.
    _detail_handle: Mutex<tokio::task::JoinHandle<()>>,
}

impl SchedulerBridge {
    /// Start the bridge against `socket_path`. Always succeeds — the
    /// background tasks handle the connect/reconnect lifecycle.
    ///
    /// Two long-lived subscribers run in parallel:
    /// - `subscribe_loop` for summary events (`Subscribe`).
    /// - `subscribe_all_details_loop` for unfiltered detail events
    ///   (`SubscribeAllDetails`).
    ///
    /// Both feed the same `event_tx` broadcast — WS clients see a
    /// single combined stream and filter by variant.
    pub async fn start(socket_path: PathBuf) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel::<SchedulerEvent>(EVENT_BROADCAST_SIZE);

        let bridge = Arc::new(Self {
            socket_path: socket_path.clone(),
            connected: AtomicBool::new(false),
            event_tx: event_tx.clone(),
            _conn_handle: Mutex::new(tokio::spawn(async {})), // placeholder
            _detail_handle: Mutex::new(tokio::spawn(async {})), // placeholder
        });

        let bridge_for_loop = bridge.clone();
        let event_tx_clone = event_tx.clone();
        let socket_clone = socket_path.clone();
        let handle = tokio::spawn(async move {
            subscribe_loop(socket_clone, event_tx_clone, bridge_for_loop).await;
        });
        *bridge._conn_handle.lock().await = handle;

        // Parallel detail subscriber. Reconnects independently —
        // a transient detail-stream blip doesn't tear down the summary
        // stream and vice versa.
        let detail_event_tx = event_tx.clone();
        let detail_socket = socket_path;
        let detail_handle = tokio::spawn(async move {
            subscribe_all_details_loop(detail_socket, detail_event_tx).await;
        });
        *bridge._detail_handle.lock().await = detail_handle;

        bridge
    }

    /// Whether the bridge is currently connected (subscribe stream
    /// alive). Read lock-free; safe to call from any thread.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Subscribe to push events from the scheduler. Each WS client
    /// gets its own receiver; lagged subscribers may see a
    /// `RecvError::Lagged`, after which the next `recv` returns the
    /// next available event (the scheduler-side server re-emits a
    /// `Snapshot` on its own lag detection so consumers can converge
    /// even without bridge-level help).
    pub fn subscribe_events(&self) -> broadcast::Receiver<SchedulerEvent> {
        self.event_tx.subscribe()
    }

    /// Send one [`ControlRequest`] over a fresh connection and wait
    /// for the corresponding [`ControlResponse`]. Errors surface as
    /// [`SchedulerBridgeError`] for easy mapping into HTTP statuses.
    ///
    /// Note: `Subscribe` is the only request that switches the
    /// scheduler-side connection into stream mode, so callers
    /// **must not** route it through `request` — it's reserved for
    /// the long-lived subscribe loop. We return
    /// [`SchedulerBridgeError::BadResponse`] if a caller forgets.
    pub async fn request(
        &self,
        req: ControlRequest,
    ) -> Result<ControlResponse, SchedulerBridgeError> {
        match req {
            ControlRequest::Subscribe => {
                return Err(SchedulerBridgeError::BadResponse(
                    "Subscribe is reserved for the bridge's own loop".into(),
                ));
            }
            ControlRequest::SubscribeAllDetails => {
                return Err(SchedulerBridgeError::BadResponse(
                    "SubscribeAllDetails is reserved for the bridge's own loop".into(),
                ));
            }
            ControlRequest::SubscribeDetail { .. } => {
                // Detail events ride on the bridge's own
                // SubscribeAllDetails stream; per-WS-client
                // SubscribeDetail connections aren't supported here.
                return Err(SchedulerBridgeError::BadResponse(
                    "SubscribeDetail is not supported through the bridge — \
                     subscribe to the bridge's event stream instead"
                        .into(),
                ));
            }
            _ => {}
        }
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            one_shot_request(&self.socket_path, req),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => Err(SchedulerBridgeError::Timeout),
        }
    }
}

/// Open a new socket, send one request, read one response, drop.
/// Pulled out so [`SchedulerBridge::request`] can wrap it in a
/// timeout without leaking the timeout into every error path.
async fn one_shot_request(
    socket_path: &Path,
    req: ControlRequest,
) -> Result<ControlResponse, SchedulerBridgeError> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|_| SchedulerBridgeError::Unreachable)?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    let json = serde_json::to_vec(&req)
        .map_err(|e| SchedulerBridgeError::BadResponse(format!("encode: {e}")))?;
    let frame = ipc::encode_frame(&json);
    writer
        .write_all(&frame)
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;

    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(SchedulerBridgeError::BadResponse(format!(
            "oversized frame: {len}"
        )));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
    serde_json::from_slice(&payload)
        .map_err(|e| SchedulerBridgeError::BadResponse(format!("decode: {e}")))
}

/// Long-lived subscribe loop. Connects, sends one Subscribe frame,
/// reads `ControlResponse::Event` frames forever, and forwards each
/// inner [`SchedulerEvent`] over the broadcast channel. Reconnects
/// with capped exponential backoff on disconnect.
async fn subscribe_loop(
    socket_path: PathBuf,
    event_tx: broadcast::Sender<SchedulerEvent>,
    bridge: Arc<SchedulerBridge>,
) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        debug!(path = %socket_path.display(), "connecting to scheduler");
        match run_subscribe_session(&socket_path, &event_tx, &bridge).await {
            Ok(()) => {
                // Clean EOF — the scheduler shut down. Reconnect with
                // INITIAL_BACKOFF so we come back fast.
                warn!("scheduler subscribe stream closed cleanly");
                backoff = INITIAL_BACKOFF;
            }
            Err(e) => {
                debug!(error = %e, "scheduler subscribe failed");
                // Tick backoff on error.
            }
        }
        bridge.connected.store(false, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One subscribe session. Returns `Ok(())` on clean EOF or
/// [`SchedulerBridgeError`] on any failure (including the initial
/// connect refusal — most common case when the scheduler isn't
/// running).
async fn run_subscribe_session(
    socket_path: &Path,
    event_tx: &broadcast::Sender<SchedulerEvent>,
    bridge: &Arc<SchedulerBridge>,
) -> Result<(), SchedulerBridgeError> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|_| SchedulerBridgeError::Unreachable)?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Send one Subscribe frame, then read events forever.
    let json = serde_json::to_vec(&ControlRequest::Subscribe)
        .map_err(|e| SchedulerBridgeError::BadResponse(format!("encode: {e}")))?;
    let frame = ipc::encode_frame(&json);
    writer
        .write_all(&frame)
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;

    bridge.connected.store(true, Ordering::Relaxed);
    info!("connected to scheduler");

    loop {
        let mut len_buf = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            // EOF when the scheduler shuts down — this is the clean
            // path, not an error.
            return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(SchedulerBridgeError::Io(e.to_string()))
            };
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(SchedulerBridgeError::BadResponse(format!(
                "oversized frame: {len}"
            )));
        }
        let mut payload = vec![0u8; len];
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
        let response: ControlResponse = serde_json::from_slice(&payload)
            .map_err(|e| SchedulerBridgeError::BadResponse(format!("decode: {e}")))?;
        match response {
            ControlResponse::Event { event } => {
                // `send` returns Err only when there are no
                // subscribers — that's fine, the scheduler keeps
                // pushing and the next WS client picks up future
                // events.
                let _ = event_tx.send(event);
            }
            other => {
                warn!(
                    response = ?other,
                    "unexpected non-Event frame on subscribe stream"
                );
            }
        }
    }
}

/// Long-lived loop that opens a [`ControlRequest::SubscribeAllDetails`]
/// connection and forwards every [`SchedulerEvent::DetailUpdated`]
/// onto the same broadcast channel summary events ride on. Reconnects
/// with capped exponential backoff, independent of the summary loop —
/// a transient blip on one stream doesn't tear down the other.
///
/// Doesn't touch `bridge.connected` — that flag tracks the summary
/// stream specifically, since it's the one that signals "scheduler
/// reachable" to the SPA. The detail stream may be momentarily down
/// while summary is up; from a UX standpoint the SPA's expanded card
/// would briefly stop receiving updates but the rest of the dashboard
/// keeps working.
async fn subscribe_all_details_loop(
    socket_path: PathBuf,
    event_tx: broadcast::Sender<SchedulerEvent>,
) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        debug!(path = %socket_path.display(), "connecting to scheduler details stream");
        match run_subscribe_all_details_session(&socket_path, &event_tx).await {
            Ok(()) => {
                warn!("scheduler details stream closed cleanly");
                backoff = INITIAL_BACKOFF;
            }
            Err(e) => {
                debug!(error = %e, "scheduler details subscribe failed");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One details-subscribe session. Mirrors [`run_subscribe_session`]
/// but writes a `SubscribeAllDetails` frame and forwards every event
/// on the broadcast channel.
async fn run_subscribe_all_details_session(
    socket_path: &Path,
    event_tx: &broadcast::Sender<SchedulerEvent>,
) -> Result<(), SchedulerBridgeError> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|_| SchedulerBridgeError::Unreachable)?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    let json = serde_json::to_vec(&ControlRequest::SubscribeAllDetails)
        .map_err(|e| SchedulerBridgeError::BadResponse(format!("encode: {e}")))?;
    let frame = ipc::encode_frame(&json);
    writer
        .write_all(&frame)
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;

    info!("connected to scheduler details stream");

    loop {
        let mut len_buf = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            return if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(SchedulerBridgeError::Io(e.to_string()))
            };
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(SchedulerBridgeError::BadResponse(format!(
                "oversized frame: {len}"
            )));
        }
        let mut payload = vec![0u8; len];
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| SchedulerBridgeError::Io(e.to_string()))?;
        let response: ControlResponse = serde_json::from_slice(&payload)
            .map_err(|e| SchedulerBridgeError::BadResponse(format!("decode: {e}")))?;
        match response {
            ControlResponse::Event { event } => {
                let _ = event_tx.send(event);
            }
            other => {
                warn!(
                    response = ?other,
                    "unexpected non-Event frame on details stream"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clhorde_core::control::WorkflowSummary;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    /// Spin up a minimal subscribe-mode mock: bind, accept one
    /// connection, drain the Subscribe frame, write the supplied
    /// events. Closes the writer after — the bridge sees EOF.
    async fn spawn_mock_subscribe(path: PathBuf, events: Vec<SchedulerEvent>) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = tokio::io::split(stream);

            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            r.read_exact(&mut payload).await.unwrap();

            for event in events {
                let resp = ControlResponse::Event { event };
                let body = serde_json::to_vec(&resp).unwrap();
                let frame = ipc::encode_frame(&body);
                w.write_all(&frame).await.unwrap();
            }
            w.flush().await.unwrap();
        });
    }

    /// Spin up a one-shot request server that always replies with
    /// the supplied response.
    async fn spawn_mock_one_shot(path: PathBuf, response: ControlResponse) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = tokio::io::split(stream);

            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf).await.unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            r.read_exact(&mut payload).await.unwrap();

            let body = serde_json::to_vec(&response).unwrap();
            let frame = ipc::encode_frame(&body);
            w.write_all(&frame).await.unwrap();
            w.flush().await.unwrap();
        });
    }

    fn sample_summary(name: &str, status: &str) -> WorkflowSummary {
        WorkflowSummary {
            name: name.into(),
            status: status.into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec![],
            blocked_by: vec![],
        }
    }

    #[tokio::test]
    async fn start_does_not_block_when_scheduler_offline() {
        // The single most important property: web server startup
        // must not be gated on a running scheduler.
        let tmp = TempDir::new().unwrap();
        let phantom = tmp.path().join("not-here.sock");

        // If this hangs, the test runner times out.
        let bridge = SchedulerBridge::start(phantom).await;
        assert!(!bridge.is_connected());
    }

    #[tokio::test]
    async fn subscribe_loop_forwards_events_to_subscribers() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        let events = vec![
            SchedulerEvent::Snapshot {
                workflows: vec![sample_summary("x", "queued")],
                root: Some("/repo".into()),
            },
            SchedulerEvent::WorkflowUpdated {
                summary: sample_summary("x", "implementing"),
            },
        ];
        spawn_mock_subscribe(path.clone(), events).await;

        let bridge = SchedulerBridge::start(path).await;
        let mut rx = bridge.subscribe_events();

        // Wait for both events. Limit to a couple seconds so a flake
        // doesn't hang CI forever.
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event arrived in time")
            .expect("first event payload");
        match first {
            SchedulerEvent::Snapshot { workflows, .. } => {
                assert_eq!(workflows.len(), 1);
                assert_eq!(workflows[0].name, "x");
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second event arrived in time")
            .expect("second event payload");
        match second {
            SchedulerEvent::WorkflowUpdated { summary } => {
                assert_eq!(summary.status, "implementing");
            }
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_returns_unreachable_when_scheduler_is_offline() {
        let tmp = TempDir::new().unwrap();
        let phantom = tmp.path().join("not-here.sock");
        let bridge = SchedulerBridge::start(phantom).await;

        let res = bridge.request(ControlRequest::Ping).await;
        assert!(matches!(res, Err(SchedulerBridgeError::Unreachable)));
    }

    #[tokio::test]
    async fn request_round_trip_via_one_shot() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        spawn_mock_one_shot(path.clone(), ControlResponse::Pong).await;

        let bridge = SchedulerBridge::start(path).await;
        let res = bridge
            .request(ControlRequest::Ping)
            .await
            .expect("Ping must succeed");
        assert!(matches!(res, ControlResponse::Pong));
    }

    #[tokio::test]
    async fn request_rejects_subscribe_to_avoid_misuse() {
        // Routing Subscribe through `request` would deadlock the
        // caller — the server never sends a one-shot response on a
        // subscribe connection. The bridge surfaces a clear error
        // instead.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        let bridge = SchedulerBridge::start(path).await;

        let res = bridge.request(ControlRequest::Subscribe).await;
        match res {
            Err(SchedulerBridgeError::BadResponse(msg)) => {
                assert!(msg.contains("Subscribe"));
            }
            other => panic!("expected BadResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_rejects_subscribe_all_details_to_avoid_misuse() {
        // Same trap as Subscribe: SubscribeAllDetails switches the
        // connection into stream mode, so a one-shot caller would
        // hang waiting for a response.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        let bridge = SchedulerBridge::start(path).await;

        let res = bridge.request(ControlRequest::SubscribeAllDetails).await;
        match res {
            Err(SchedulerBridgeError::BadResponse(msg)) => {
                assert!(msg.contains("SubscribeAllDetails"));
            }
            other => panic!("expected BadResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_rejects_per_workflow_subscribe_detail() {
        // Per-workflow SubscribeDetail isn't supported through the
        // bridge — clients consume the bridge's combined event
        // stream and filter on the SPA side.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        let bridge = SchedulerBridge::start(path).await;

        let res = bridge
            .request(ControlRequest::SubscribeDetail {
                name: "x".into(),
            })
            .await;
        match res {
            Err(SchedulerBridgeError::BadResponse(msg)) => {
                assert!(msg.contains("SubscribeDetail"));
            }
            other => panic!("expected BadResponse, got {other:?}"),
        }
    }

    /// Mock server that handles BOTH the Subscribe and
    /// SubscribeAllDetails connections the bridge opens in parallel.
    /// Routes by inspecting the request frame and replies with the
    /// corresponding events.
    async fn spawn_mock_dual_subscribe(
        path: PathBuf,
        summary_events: Vec<SchedulerEvent>,
        detail_events: Vec<SchedulerEvent>,
    ) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            // Accept both connections; spawn a per-connection handler.
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let summary_clone = summary_events.clone();
                let detail_clone = detail_events.clone();
                tokio::spawn(async move {
                    let (mut r, mut w) = tokio::io::split(stream);
                    let mut len_buf = [0u8; 4];
                    r.read_exact(&mut len_buf).await.unwrap();
                    let len = u32::from_be_bytes(len_buf) as usize;
                    let mut payload = vec![0u8; len];
                    r.read_exact(&mut payload).await.unwrap();
                    let req: ControlRequest = serde_json::from_slice(&payload).unwrap();

                    let events = match req {
                        ControlRequest::Subscribe => summary_clone,
                        ControlRequest::SubscribeAllDetails => detail_clone,
                        other => panic!("unexpected request: {other:?}"),
                    };
                    for event in events {
                        let resp = ControlResponse::Event { event };
                        let body = serde_json::to_vec(&resp).unwrap();
                        let frame = ipc::encode_frame(&body);
                        w.write_all(&frame).await.unwrap();
                    }
                    w.flush().await.unwrap();
                });
            }
            // Keep the listener alive until both connections finish.
        });
    }

    #[tokio::test]
    async fn details_stream_forwards_detail_events_to_subscribers() {
        // The bridge's parallel SubscribeAllDetails loop must
        // forward DetailUpdated frames into the same broadcast as
        // summary events so WS clients see one combined stream.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");

        let detail = clhorde_core::control::WorkflowDetail {
            name: "alpha".into(),
            status: "implementing".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            apply: vec![],
            verify: None,
            archive: None,
            blocked_by: vec![],
        };

        spawn_mock_dual_subscribe(
            path.clone(),
            vec![],
            vec![SchedulerEvent::DetailUpdated {
                detail: detail.clone(),
            }],
        )
        .await;

        let bridge = SchedulerBridge::start(path).await;
        let mut rx = bridge.subscribe_events();

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event arrived in time")
            .expect("event payload");
        match evt {
            SchedulerEvent::DetailUpdated { detail: got } => {
                assert_eq!(got, detail);
            }
            other => panic!("expected DetailUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summary_and_detail_streams_share_one_broadcast() {
        // Multiplexing both kinds onto one channel is the
        // load-bearing simplification — without it WS handlers would
        // need to manage two receivers each. The test asserts that
        // both kinds of events surface on a single subscribe_events
        // receiver.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");

        let summary = sample_summary("alpha", "queued");
        let detail = clhorde_core::control::WorkflowDetail {
            name: "alpha".into(),
            status: "queued".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            apply: vec![],
            verify: None,
            archive: None,
            blocked_by: vec![],
        };

        spawn_mock_dual_subscribe(
            path.clone(),
            vec![SchedulerEvent::WorkflowUpdated {
                summary: summary.clone(),
            }],
            vec![SchedulerEvent::DetailUpdated {
                detail: detail.clone(),
            }],
        )
        .await;

        let bridge = SchedulerBridge::start(path).await;
        let mut rx = bridge.subscribe_events();

        let mut got_summary = false;
        let mut got_detail = false;
        for _ in 0..2 {
            let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("event arrived in time")
                .expect("event payload");
            match evt {
                SchedulerEvent::WorkflowUpdated { .. } => got_summary = true,
                SchedulerEvent::DetailUpdated { .. } => got_detail = true,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(got_summary, "summary stream did not deliver");
        assert!(got_detail, "details stream did not deliver");
    }
}
