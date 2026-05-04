//! Control-socket server hosted inside the scheduler `daemon`
//! subcommand.
//!
//! The server holds a shared reference to the running [`Orchestrator`]
//! (wrapped in a `std::sync::Mutex` because every orchestrator method is
//! synchronous — there are no `.await`s inside) and answers
//! [`ControlRequest`]s by mutating it under the lock, then writing back
//! a [`ControlResponse`] frame.
//!
//! The dispatch logic ([`dispatch_request`]) is split out as a pure
//! function so unit tests can exercise it without spinning up a
//! `UnixListener`. The frame loop ([`handle_client`]) is reachable
//! through [`run_with_streams`], which takes any pair of
//! `AsyncRead + AsyncWrite` halves — that's what the test harness uses
//! via `tokio::io::duplex`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use clhorde_core::ipc::{self, MAX_FRAME_SIZE};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::protocol::{ControlRequest, ControlResponse, SchedulerEvent};
use crate::orchestrator::{Orchestrator, OrchestratorError};

/// Spawn the control-socket accept loop. Removes any stale socket file
/// at `socket_path` before binding (mirroring the daemon). The returned
/// [`JoinHandle`] resolves only on accept-loop failure or process exit.
pub fn spawn(
    orch: Arc<Mutex<Orchestrator>>,
    socket_path: PathBuf,
) -> std::io::Result<JoinHandle<()>> {
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    info!(socket = %socket_path.display(), "scheduler control socket listening");

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "scheduler control accept failed");
                    continue;
                }
            };
            let orch = orch.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_unix_stream(stream, orch).await {
                    debug!(error = %e, "scheduler control client ended");
                }
            });
        }
    });
    Ok(handle)
}

async fn serve_unix_stream(
    stream: UnixStream,
    orch: Arc<Mutex<Orchestrator>>,
) -> std::io::Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    run_with_streams(reader, writer, orch).await
}

/// Read/write loop driven from any AsyncRead+AsyncWrite pair. Exposed
/// so tests can plumb both halves of a `tokio::io::duplex` and still
/// exercise the framing path. Returns when the client closes the read
/// half or sends an unrecoverable framing error.
///
/// On [`ControlRequest::Subscribe`], the connection switches into
/// stream mode: an initial [`SchedulerEvent::Snapshot`] is written,
/// then orchestrator events are forwarded until the client closes the
/// read half. No further requests are accepted on a subscribe
/// connection — clients open a separate one-shot connection for
/// follow-up actions (Q/X/T/Detail/…).
pub async fn run_with_streams<R, W>(
    mut reader: R,
    mut writer: W,
    orch: Arc<Mutex<Orchestrator>>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let mut len_buf = [0u8; 4];
        if reader.read_exact(&mut len_buf).await.is_err() {
            return Ok(()); // EOF — client closed the socket cleanly.
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("oversized control frame: {len}"),
            ));
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;

        match serde_json::from_slice::<ControlRequest>(&payload) {
            Ok(ControlRequest::Subscribe) => {
                // Hand off to the streaming branch and never come back —
                // this connection is one-way after Subscribe.
                return run_subscribe_stream(reader, writer, orch).await;
            }
            Ok(ControlRequest::SubscribeDetail { name }) => {
                // Same one-way handoff as Subscribe, but scoped to one
                // workflow's detail.
                return run_subscribe_detail_stream(reader, writer, orch, name).await;
            }
            Ok(ControlRequest::SubscribeAllDetails) => {
                // Unfiltered detail stream for the web bridge; same
                // one-way handoff.
                return run_subscribe_all_details_stream(reader, writer, orch).await;
            }
            Ok(req) => {
                let response = {
                    let mut guard = orch
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    dispatch_request(&mut guard, req)
                };
                write_response(&mut writer, &response).await?;
            }
            Err(e) => {
                let response = ControlResponse::Error {
                    message: format!("malformed request: {e}"),
                };
                write_response(&mut writer, &response).await?;
            }
        }
    }
}

/// Encode + write one [`ControlResponse`] on `writer`. Small helper so
/// the request branch and the subscribe branch share one frame-writing
/// path (and one fallback for the unreachable serde failure mode).
async fn write_response<W>(
    writer: &mut W,
    response: &ControlResponse,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let json = serde_json::to_vec(response).unwrap_or_else(|e| {
        // serde_json on our owned types should never fail; if it does,
        // send a minimal error to avoid hanging the client.
        format!(r#"{{"type":"error","message":"serialize: {e}"}}"#).into_bytes()
    });
    let frame = ipc::encode_frame(&json);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Stream-mode handler for a Subscribe connection.
///
/// 1. Take a fresh `broadcast::Receiver` from the orchestrator
///    *before* snapshotting state, so any event fired between the
///    snapshot and the first `recv` is queued for delivery (no race).
/// 2. Snapshot every workflow + the watched root and write one
///    [`SchedulerEvent::Snapshot`] frame.
/// 3. Loop forwarding [`SchedulerEvent::WorkflowUpdated`] frames as
///    they arrive on the broadcast channel. On `RecvError::Lagged`,
///    re-emit a fresh Snapshot so a slow client always converges back
///    to consistent state instead of staying out of sync.
/// 4. Keep reading from `reader` only to detect when the client
///    closes the socket; any further data on a Subscribe connection
///    is ignored (it can't be honoured here without breaking the
///    one-way contract).
async fn run_subscribe_stream<R, W>(
    mut reader: R,
    mut writer: W,
    orch: Arc<Mutex<Orchestrator>>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Subscribe BEFORE the snapshot so we don't drop events that fire
    // between the snapshot and the first `recv`. The receiver will
    // hold those events in the broadcast buffer until we read them.
    let mut events = {
        let guard = orch.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.events_subscribe()
    };

    let snapshot = {
        let guard = orch.lock().unwrap_or_else(|poison| poison.into_inner());
        SchedulerEvent::Snapshot {
            workflows: guard.summaries(),
            root: Some(guard.root().to_string_lossy().into_owned()),
        }
    };
    write_response(
        &mut writer,
        &ControlResponse::Event { event: snapshot },
    )
    .await?;

    // Drain any garbage the client sends on this connection — we just
    // need to detect EOF so we can shut the writer down cleanly.
    let mut sink = [0u8; 1024];

    loop {
        tokio::select! {
            event_res = events.recv() => {
                match event_res {
                    Ok(event) => {
                        if let Err(e) = write_response(
                            &mut writer,
                            &ControlResponse::Event { event },
                        )
                        .await
                        {
                            debug!(error = %e, "subscribe write failed");
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Slow subscriber lost messages. Re-send a
                        // fresh snapshot so the client converges back
                        // to a consistent baseline.
                        let snap = {
                            let guard = orch
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            SchedulerEvent::Snapshot {
                                workflows: guard.summaries(),
                                root: Some(
                                    guard.root().to_string_lossy().into_owned(),
                                ),
                            }
                        };
                        if let Err(e) = write_response(
                            &mut writer,
                            &ControlResponse::Event { event: snap },
                        )
                        .await
                        {
                            debug!(error = %e, "subscribe lag-snapshot write failed");
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Orchestrator was dropped. End the stream.
                        return Ok(());
                    }
                }
            }
            read_res = reader.read(&mut sink) => {
                match read_res {
                    Ok(0) => return Ok(()), // EOF — client gone.
                    Ok(_) => {} // ignore extra bytes on a subscribe socket
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// Stream-mode handler for a [`ControlRequest::SubscribeDetail`]
/// connection.
///
/// 1. Subscribe to the orchestrator's detail-event broadcast *before*
///    snapshotting state (same race-avoidance trick as
///    [`run_subscribe_stream`]).
/// 2. Look up the workflow's [`WorkflowDetail`]. On miss, write one
///    [`ControlResponse::Error`] frame and return — the connection
///    never enters stream mode and emits no events.
/// 3. On hit, write one [`SchedulerEvent::DetailSnapshot`] frame.
/// 4. Loop forwarding [`SchedulerEvent::DetailUpdated`] frames as
///    they arrive on the broadcast, filtered to events whose
///    `detail.name == name`. On `RecvError::Lagged`, re-emit a fresh
///    `DetailSnapshot` so a slow client converges back to consistent
///    state. If the workflow goes away after subscribing, the next
///    re-snapshot returns `None` and we close the stream — same shape
///    a missing workflow would take at subscription time.
/// 5. Keep reading from `reader` only to detect EOF; extra bytes on a
///    detail-subscribe connection are dropped (one-way contract).
async fn run_subscribe_detail_stream<R, W>(
    mut reader: R,
    mut writer: W,
    orch: Arc<Mutex<Orchestrator>>,
    name: String,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut events = {
        let guard = orch.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.detail_events_subscribe()
    };

    let initial = {
        let guard = orch.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.detail(&name)
    };
    let detail = match initial {
        Some(d) => d,
        None => {
            let resp = ControlResponse::Error {
                message: format!("no such workflow: {name}"),
            };
            write_response(&mut writer, &resp).await?;
            return Ok(());
        }
    };

    write_response(
        &mut writer,
        &ControlResponse::Event {
            event: SchedulerEvent::DetailSnapshot { detail },
        },
    )
    .await?;

    let mut sink = [0u8; 1024];

    loop {
        tokio::select! {
            event_res = events.recv() => {
                match event_res {
                    Ok(SchedulerEvent::DetailUpdated { detail }) if detail.name == name => {
                        if let Err(e) = write_response(
                            &mut writer,
                            &ControlResponse::Event {
                                event: SchedulerEvent::DetailUpdated { detail },
                            },
                        )
                        .await
                        {
                            debug!(error = %e, "subscribe_detail write failed");
                            return Ok(());
                        }
                    }
                    // Detail-broadcast events for other workflows or
                    // unrelated kinds: silently drop — this connection
                    // only forwards `name`'s detail.
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Slow subscriber lost messages. Re-send a
                        // fresh snapshot so the client reconverges.
                        let snap = {
                            let guard = orch
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            guard.detail(&name)
                        };
                        match snap {
                            Some(detail) => {
                                if let Err(e) = write_response(
                                    &mut writer,
                                    &ControlResponse::Event {
                                        event: SchedulerEvent::DetailSnapshot { detail },
                                    },
                                )
                                .await
                                {
                                    debug!(error = %e, "subscribe_detail lag-snapshot write failed");
                                    return Ok(());
                                }
                            }
                            None => {
                                // Workflow disappeared while we were
                                // lagging. End the stream — there's
                                // nothing left to track.
                                return Ok(());
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Ok(());
                    }
                }
            }
            read_res = reader.read(&mut sink) => {
                match read_res {
                    Ok(0) => return Ok(()),
                    Ok(_) => {} // ignore stray bytes on a one-way socket
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// Stream-mode handler for a [`ControlRequest::SubscribeAllDetails`]
/// connection. Forwards every [`SchedulerEvent::DetailUpdated`] the
/// orchestrator emits — no filter, no initial snapshot. Designed for
/// the web bridge to fan out to many WS clients that filter
/// per-workflow on the SPA side.
async fn run_subscribe_all_details_stream<R, W>(
    mut reader: R,
    mut writer: W,
    orch: Arc<Mutex<Orchestrator>>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut events = {
        let guard = orch.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.detail_events_subscribe()
    };

    let mut sink = [0u8; 1024];

    loop {
        tokio::select! {
            event_res = events.recv() => {
                match event_res {
                    Ok(event) => {
                        if let Err(e) = write_response(
                            &mut writer,
                            &ControlResponse::Event { event },
                        )
                        .await
                        {
                            debug!(error = %e, "subscribe_all_details write failed");
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // The bridge re-resolves expanded cards via REST
                        // when a user opens them, so a missed
                        // DetailUpdated is recoverable on the next
                        // mutation. Log and keep streaming rather than
                        // close the connection.
                        debug!("subscribe_all_details lag — events dropped");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Ok(());
                    }
                }
            }
            read_res = reader.read(&mut sink) => {
                match read_res {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// Apply one request to the orchestrator and return the response.
/// Pure dispatch — no I/O, no awaits — so tests can hit it directly.
pub fn dispatch_request(
    orch: &mut Orchestrator,
    req: ControlRequest,
) -> ControlResponse {
    let root = Some(orch.root().to_string_lossy().into_owned());
    match req {
        ControlRequest::Ping => ControlResponse::Pong,
        ControlRequest::Status { name: None } => ControlResponse::Status {
            workflows: orch.summaries(),
            root,
        },
        ControlRequest::Status { name: Some(n) } => match orch.summary(&n) {
            Some(s) => ControlResponse::Status {
                workflows: vec![s],
                root,
            },
            None => ControlResponse::Error {
                message: format!("no such workflow: {n}"),
            },
        },
        ControlRequest::Cancel { name } => match orch.cancel_workflow(&name) {
            Ok(kind) => ControlResponse::Ok {
                message: format!("{name}: {kind}"),
            },
            Err(e) => map_err_to_response(e),
        },
        ControlRequest::Retry { name, section } => {
            match orch.retry_section(&name, &section) {
                Ok(()) => ControlResponse::Ok {
                    message: format!("retry dispatched: {name} section {section}"),
                },
                Err(e) => map_err_to_response(e),
            }
        }
        ControlRequest::Queue { name, priority } => {
            match orch.queue_workflow(&name, priority) {
                Ok(()) => ControlResponse::Ok {
                    message: format!("queued: {name}"),
                },
                Err(e) => map_err_to_response(e),
            }
        }
        ControlRequest::Detail { name } => match orch.detail(&name) {
            Some(detail) => ControlResponse::Detail { detail },
            None => ControlResponse::Error {
                message: format!("no such workflow: {name}"),
            },
        },
        // Subscribe never reaches dispatch_request in production — the
        // run_with_streams loop branches on it and switches into
        // stream-mode before re-entering this function. We surface a
        // clear error in case a future code path forgets that.
        ControlRequest::Subscribe => ControlResponse::Error {
            message: "subscribe not supported on a one-shot connection".into(),
        },
        ControlRequest::SubscribeDetail { .. } => ControlResponse::Error {
            message: "subscribe_detail not supported on a one-shot connection".into(),
        },
        ControlRequest::SubscribeAllDetails => ControlResponse::Error {
            message: "subscribe_all_details not supported on a one-shot connection".into(),
        },
    }
}

fn map_err_to_response(e: OrchestratorError) -> ControlResponse {
    ControlResponse::Error {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openspec::discovery::MarkerMetadata;
    use crate::persistence::WorkflowStore;
    use crate::workflow::Workflow;
    use clhorde_core::protocol::ClientRequest;
    use std::fs;
    use tempfile::TempDir;
    use tokio::io::{duplex, AsyncWriteExt};
    use tokio::sync::mpsc;

    fn fixture() -> (
        TempDir,
        Arc<Mutex<Orchestrator>>,
        mpsc::UnboundedReceiver<ClientRequest>,
    ) {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("store"));
        let (tx, rx) = mpsc::unbounded_channel();
        let orch = Orchestrator::new(tmp.path(), store, tx);
        (tmp, Arc::new(Mutex::new(orch)), rx)
    }

    fn change_dir(tmp: &TempDir, name: &str) -> std::path::PathBuf {
        let p = tmp.path().join("openspec").join("changes").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Encode a request into a length-prefixed frame (helper for the
    /// duplex tests).
    fn encode_request(req: &ControlRequest) -> Vec<u8> {
        let json = serde_json::to_vec(req).unwrap();
        ipc::encode_frame(&json)
    }

    /// Pull one length-prefixed JSON response off `reader`.
    async fn read_response<R: AsyncRead + Unpin>(
        reader: &mut R,
    ) -> ControlResponse {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await.unwrap();
        serde_json::from_slice(&payload).unwrap()
    }

    // ── pure dispatch ──

    #[test]
    fn dispatch_ping_returns_pong() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(&mut g, ControlRequest::Ping);
        assert!(matches!(resp, ControlResponse::Pong));
    }

    #[test]
    fn dispatch_status_empty() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(&mut g, ControlRequest::Status { name: None });
        match resp {
            ControlResponse::Status { workflows, .. } => assert!(workflows.is_empty()),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_status_after_marker_lists_workflow() {
        let (tmp, orch, _rx) = fixture();
        // Discovery sees the change → reconcile inserts a Queued workflow.
        change_dir(&tmp, "x");
        fs::write(
            tmp.path().join("openspec/changes/x/.clhorde-ready"),
            "",
        )
        .unwrap();
        {
            let mut g = orch.lock().unwrap();
            g.reconcile().unwrap();
        }

        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(&mut g, ControlRequest::Status { name: None });
        match resp {
            ControlResponse::Status { workflows, .. } => {
                assert_eq!(workflows.len(), 1);
                assert_eq!(workflows[0].name, "x");
                assert_eq!(workflows[0].status, "queued");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_status_named_returns_error_for_unknown() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Status {
                name: Some("ghost".into()),
            },
        );
        match resp {
            ControlResponse::Error { message } => assert!(message.contains("ghost")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_cancel_unqueues_then_returns_ok() {
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        fs::write(
            tmp.path().join("openspec/changes/x/.clhorde-ready"),
            "",
        )
        .unwrap();
        {
            let mut g = orch.lock().unwrap();
            g.reconcile().unwrap();
        }

        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Cancel { name: "x".into() },
        );
        match resp {
            ControlResponse::Ok { message } => assert!(message.contains("unqueued")),
            other => panic!("expected Ok, got {other:?}"),
        }
        // Marker file is gone.
        assert!(!tmp
            .path()
            .join("openspec/changes/x/.clhorde-ready")
            .exists());
    }

    #[test]
    fn dispatch_cancel_unknown_workflow_errors() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Cancel {
                name: "ghost".into(),
            },
        );
        match resp {
            ControlResponse::Error { message } => assert!(message.contains("ghost")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_retry_unknown_workflow_errors() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Retry {
                name: "ghost".into(),
                section: "1".into(),
            },
        );
        match resp {
            ControlResponse::Error { message } => assert!(message.contains("ghost")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_retry_dispatches_via_outbound_channel() {
        // Persist a Failed workflow + a parseable tasks.md to disk, then
        // build the orchestrator on top so reconcile picks them up.
        let tmp = TempDir::new().unwrap();
        change_dir(&tmp, "x");
        fs::write(
            tmp.path().join("openspec/changes/x/tasks.md"),
            "## 1. A\n- [ ] 1.1 a\n",
        )
        .unwrap();
        let store = WorkflowStore::open(tmp.path().join("store"));
        let mut wf = Workflow::drafted("x");
        wf.queue(MarkerMetadata::default()).unwrap();
        wf.start_implementing().unwrap();
        wf.fail("section 1 retried out").unwrap();
        store.save(&wf).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<ClientRequest>();
        let orch = Arc::new(Mutex::new(Orchestrator::new(
            tmp.path(),
            store,
            tx,
        )));
        {
            let mut g = orch.lock().unwrap();
            g.reconcile().unwrap();
        }

        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Retry {
                name: "x".into(),
                section: "1".into(),
            },
        );
        match resp {
            ControlResponse::Ok { message } => assert!(message.contains("section 1")),
            other => panic!("expected Ok, got {other:?}"),
        }
        drop(g);

        let req = rx.try_recv().unwrap();
        match req {
            ClientRequest::SubmitPrompt { tags, .. } => {
                assert!(tags
                    .iter()
                    .any(|t| t.contains("phase=apply") && t.contains("/node=1")));
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_queue_writes_marker_and_reports_ok() {
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Queue {
                name: "x".into(),
                priority: Some(7),
            },
        );
        match resp {
            ControlResponse::Ok { message } => assert!(message.contains("queued: x")),
            other => panic!("expected Ok, got {other:?}"),
        }
        let marker = tmp.path().join("openspec/changes/x/.clhorde-ready");
        let body = fs::read_to_string(&marker).unwrap();
        assert!(body.contains("priority = 7"));
        // Workflow snapshot now reports "queued".
        let summaries = g.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "x");
        assert_eq!(summaries[0].status, "queued");
    }

    #[test]
    fn dispatch_queue_unknown_change_errors() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Queue {
                name: "ghost".into(),
                priority: None,
            },
        );
        match resp {
            ControlResponse::Error { message } => assert!(message.contains("ghost")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_detail_unknown_workflow_errors() {
        let (_tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Detail {
                name: "ghost".into(),
            },
        );
        match resp {
            ControlResponse::Error { message } => assert!(message.contains("ghost")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_detail_returns_workflow_with_empty_apply_before_parse() {
        // A Drafted workflow has no DAG yet. The Detail response should
        // still come back successfully — apply just empty — so the TUI
        // can render the placeholder body.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        {
            let mut g = orch.lock().unwrap();
            g.reconcile().unwrap();
        }
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(
            &mut g,
            ControlRequest::Detail { name: "x".into() },
        );
        match resp {
            ControlResponse::Detail { detail } => {
                assert_eq!(detail.name, "x");
                assert_eq!(detail.status, "drafted");
                assert!(detail.apply.is_empty());
                assert!(detail.verify.is_none());
                assert!(detail.archive.is_none());
            }
            other => panic!("expected Detail, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_status_carries_root_path() {
        // The TUI/web bridge uses the root field to resolve
        // openspec/changes/<name>/proposal.md and to seed cwd for the
        // "continue exploring" action — make sure the server populates
        // it on every Status reply.
        let (tmp, orch, _rx) = fixture();
        let mut g = orch.lock().unwrap();
        let resp = dispatch_request(&mut g, ControlRequest::Status { name: None });
        match resp {
            ControlResponse::Status { root, .. } => {
                let r = root.expect("root must be present");
                assert_eq!(std::path::PathBuf::from(r), tmp.path());
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    // ── duplex framing path ──

    #[tokio::test]
    async fn ping_round_trip_via_duplex() {
        let (_tmp, orch, _rx) = fixture();
        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let server_orch = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, server_orch).await;
        });

        // Write a Ping frame.
        let frame = encode_request(&ControlRequest::Ping);
        client.write_all(&frame).await.unwrap();
        client.flush().await.unwrap();

        let resp = read_response(&mut client).await;
        assert!(matches!(resp, ControlResponse::Pong));

        drop(client); // EOF the server.
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn malformed_request_yields_error_response() {
        let (_tmp, orch, _rx) = fixture();
        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch).await;
        });

        // Send garbage JSON inside a valid frame.
        let frame = ipc::encode_frame(b"{not json}");
        client.write_all(&frame).await.unwrap();
        client.flush().await.unwrap();

        let resp = read_response(&mut client).await;
        match resp {
            ControlResponse::Error { message } => {
                assert!(message.contains("malformed request"))
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn back_to_back_requests_on_one_connection() {
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        fs::write(
            tmp.path().join("openspec/changes/x/.clhorde-ready"),
            "",
        )
        .unwrap();
        {
            let mut g = orch.lock().unwrap();
            g.reconcile().unwrap();
        }

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch).await;
        });

        // 1. Ping → Pong.
        client
            .write_all(&encode_request(&ControlRequest::Ping))
            .await
            .unwrap();
        // 2. Status → one workflow.
        client
            .write_all(&encode_request(&ControlRequest::Status { name: None }))
            .await
            .unwrap();
        client.flush().await.unwrap();

        match read_response(&mut client).await {
            ControlResponse::Pong => {}
            other => panic!("expected Pong, got {other:?}"),
        }
        match read_response(&mut client).await {
            ControlResponse::Status { workflows, .. } => {
                assert_eq!(workflows.len(), 1);
            }
            other => panic!("expected Status, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }

    // ── Phase 5.1: subscribe stream ──

    /// Read one Event frame off `reader`. Panics on framing errors so
    /// failures surface as test failures, not hangs.
    async fn read_event<R>(reader: &mut R) -> SchedulerEvent
    where
        R: AsyncRead + Unpin,
    {
        match read_response(reader).await {
            ControlResponse::Event { event } => event,
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_emits_initial_snapshot_then_updates() {
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        // Pre-existing workflow so the initial snapshot has content.
        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", Some(2)).unwrap();
        }

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let orch_for_server = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch_for_server).await;
        });

        // Subscribe → initial Snapshot frame must arrive first.
        client
            .write_all(&encode_request(&ControlRequest::Subscribe))
            .await
            .unwrap();
        client.flush().await.unwrap();

        match read_event(&mut client).await {
            SchedulerEvent::Snapshot { workflows, root } => {
                assert_eq!(workflows.len(), 1);
                assert_eq!(workflows[0].name, "x");
                assert_eq!(workflows[0].priority, 2);
                let r = root.expect("snapshot must carry root");
                assert_eq!(std::path::PathBuf::from(r), tmp.path());
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }

        // Mutate the orchestrator → expect a WorkflowUpdated event.
        // Cancel transitions Queued → Drafted.
        {
            let mut g = orch.lock().unwrap();
            g.cancel_workflow("x").unwrap();
        }

        match read_event(&mut client).await {
            SchedulerEvent::WorkflowUpdated { summary } => {
                assert_eq!(summary.name, "x");
                assert_eq!(summary.status, "drafted");
            }
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }

        drop(client); // EOF the server.
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn subscribe_works_with_multiple_clients() {
        // Two subscribers must each receive the same WorkflowUpdated
        // frame after a single mutation. Confirms the broadcast
        // channel really fans out from the server side.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");

        let (mut a, sa) = duplex(8192);
        let (mut b, sb) = duplex(8192);
        let (sa_r, sa_w) = tokio::io::split(sa);
        let (sb_r, sb_w) = tokio::io::split(sb);
        let orch_a = orch.clone();
        let orch_b = orch.clone();
        let task_a = tokio::spawn(async move {
            let _ = run_with_streams(sa_r, sa_w, orch_a).await;
        });
        let task_b = tokio::spawn(async move {
            let _ = run_with_streams(sb_r, sb_w, orch_b).await;
        });

        a.write_all(&encode_request(&ControlRequest::Subscribe))
            .await
            .unwrap();
        b.write_all(&encode_request(&ControlRequest::Subscribe))
            .await
            .unwrap();
        a.flush().await.unwrap();
        b.flush().await.unwrap();

        // Drain initial Snapshot on both.
        let _ = read_event(&mut a).await;
        let _ = read_event(&mut b).await;

        // Mutate.
        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", None).unwrap();
        }

        // Both subscribers must observe the Updated event.
        match read_event(&mut a).await {
            SchedulerEvent::WorkflowUpdated { summary } => {
                assert_eq!(summary.status, "queued");
            }
            other => panic!("expected WorkflowUpdated on a, got {other:?}"),
        }
        match read_event(&mut b).await {
            SchedulerEvent::WorkflowUpdated { summary } => {
                assert_eq!(summary.status, "queued");
            }
            other => panic!("expected WorkflowUpdated on b, got {other:?}"),
        }

        drop(a);
        drop(b);
        let _ = task_a.await;
        let _ = task_b.await;
    }

    // ── Phase 5.3: subscribe_detail stream ──

    #[tokio::test]
    async fn subscribe_detail_emits_initial_snapshot_then_updates() {
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        // Pre-existing workflow so the initial detail has content.
        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", Some(3)).unwrap();
        }

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let orch_for_server = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch_for_server).await;
        });

        client
            .write_all(&encode_request(&ControlRequest::SubscribeDetail {
                name: "x".into(),
            }))
            .await
            .unwrap();
        client.flush().await.unwrap();

        match read_event(&mut client).await {
            SchedulerEvent::DetailSnapshot { detail } => {
                assert_eq!(detail.name, "x");
                assert_eq!(detail.status, "queued");
                assert_eq!(detail.priority, 3);
            }
            other => panic!("expected DetailSnapshot, got {other:?}"),
        }

        // Mutate — Cancel transitions Queued → Drafted, which alters
        // the detail's status field. Expect a single DetailUpdated.
        {
            let mut g = orch.lock().unwrap();
            g.cancel_workflow("x").unwrap();
        }

        match read_event(&mut client).await {
            SchedulerEvent::DetailUpdated { detail } => {
                assert_eq!(detail.name, "x");
                assert_eq!(detail.status, "drafted");
            }
            other => panic!("expected DetailUpdated, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn subscribe_detail_filters_other_workflows() {
        // A subscriber on workflow `x` must never see DetailUpdated
        // frames for workflow `y`. Critical for keeping a
        // workflow-specific viewer quiet when unrelated workflows
        // churn.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        change_dir(&tmp, "y");
        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", None).unwrap();
        }

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let orch_for_server = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch_for_server).await;
        });

        client
            .write_all(&encode_request(&ControlRequest::SubscribeDetail {
                name: "x".into(),
            }))
            .await
            .unwrap();
        client.flush().await.unwrap();

        // Drain initial DetailSnapshot.
        match read_event(&mut client).await {
            SchedulerEvent::DetailSnapshot { detail } => {
                assert_eq!(detail.name, "x");
            }
            other => panic!("expected DetailSnapshot, got {other:?}"),
        }

        // Mutate y first (filter must drop this), then x (must surface).
        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("y", None).unwrap();
            g.cancel_workflow("x").unwrap();
        }

        match read_event(&mut client).await {
            SchedulerEvent::DetailUpdated { detail } => {
                assert_eq!(detail.name, "x");
                assert_eq!(detail.status, "drafted");
            }
            other => panic!("expected DetailUpdated for x, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn subscribe_detail_unknown_workflow_returns_error_then_eof() {
        // A SubscribeDetail against a name the orchestrator doesn't
        // know must yield exactly one Error frame and then close.
        // Otherwise a buggy client that subscribes optimistically
        // would never know the connection is dead.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "exists");

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch).await;
        });

        client
            .write_all(&encode_request(&ControlRequest::SubscribeDetail {
                name: "missing".into(),
            }))
            .await
            .unwrap();
        client.flush().await.unwrap();

        match read_response(&mut client).await {
            ControlResponse::Error { message } => {
                assert!(message.contains("missing"));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // Server must have closed by now — any further read returns EOF.
        let mut buf = [0u8; 4];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "expected EOF after Error frame, got {n} bytes");

        drop(client);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn subscribe_detail_ignores_extra_frames() {
        // Same one-way contract as Subscribe: stray frames after
        // SubscribeDetail are dropped, the server keeps streaming
        // detail events.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        // SubscribeDetail looks up an existing workflow — reconcile so
        // the FS-discovered change lands in the orchestrator's map
        // (otherwise SubscribeDetail returns Error before any stream).
        {
            let mut g = orch.lock().unwrap();
            g.reconcile().unwrap();
        }

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let orch_for_server = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch_for_server).await;
        });

        client
            .write_all(&encode_request(&ControlRequest::SubscribeDetail {
                name: "x".into(),
            }))
            .await
            .unwrap();
        let _ = read_event(&mut client).await; // initial snapshot

        // Stray Ping — must be silently dropped.
        client
            .write_all(&encode_request(&ControlRequest::Ping))
            .await
            .unwrap();
        client.flush().await.unwrap();

        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", None).unwrap();
        }

        match read_event(&mut client).await {
            SchedulerEvent::DetailUpdated { detail } => {
                assert_eq!(detail.name, "x");
                assert_eq!(detail.status, "queued");
            }
            other => panic!("expected DetailUpdated, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn subscribe_all_details_forwards_every_workflow() {
        // The unfiltered detail stream forwards DetailUpdated for any
        // workflow that mutates — no per-name filter. Two queues on
        // different workflows produce two DetailUpdated frames.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");
        change_dir(&tmp, "y");

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let orch_for_server = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch_for_server).await;
        });

        client
            .write_all(&encode_request(&ControlRequest::SubscribeAllDetails))
            .await
            .unwrap();
        client.flush().await.unwrap();

        // The server-side subscribe happens asynchronously after the
        // SubscribeAllDetails frame lands. Unlike `SubscribeDetail`
        // there's no initial snapshot to use as a sync barrier — wait
        // for the broadcast to register the new subscriber so any
        // events we emit below actually reach the stream.
        for _ in 0..50 {
            if orch.lock().unwrap().detail_event_subscriber_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            orch.lock().unwrap().detail_event_subscriber_count() > 0,
            "server never subscribed to the detail broadcast"
        );

        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", None).unwrap();
            g.queue_workflow("y", None).unwrap();
        }

        let mut names = Vec::new();
        for _ in 0..2 {
            match read_event(&mut client).await {
                SchedulerEvent::DetailUpdated { detail } => {
                    names.push(detail.name);
                }
                other => panic!("expected DetailUpdated, got {other:?}"),
            }
        }
        names.sort();
        assert_eq!(names, vec!["x".to_string(), "y".to_string()]);

        drop(client);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn subscribe_ignores_extra_frames_after_subscribe() {
        // The wire contract on Subscribe is one-way: a client that
        // accidentally sends another request after Subscribe must not
        // get a one-shot reply (which would race with the event
        // stream). We just continue streaming events and never speak
        // a Pong.
        let (tmp, orch, _rx) = fixture();
        change_dir(&tmp, "x");

        let (mut client, server) = duplex(8192);
        let (sr, sw) = tokio::io::split(server);
        let orch_for_server = orch.clone();
        let server_task = tokio::spawn(async move {
            let _ = run_with_streams(sr, sw, orch_for_server).await;
        });

        client
            .write_all(&encode_request(&ControlRequest::Subscribe))
            .await
            .unwrap();
        // Drain initial Snapshot.
        let _ = read_event(&mut client).await;

        // Send a stray Ping. The server should swallow it and keep
        // streaming events instead of replying with Pong.
        client
            .write_all(&encode_request(&ControlRequest::Ping))
            .await
            .unwrap();
        client.flush().await.unwrap();

        // Now mutate — the next frame the server emits must be the
        // WorkflowUpdated, not a Pong.
        {
            let mut g = orch.lock().unwrap();
            g.queue_workflow("x", None).unwrap();
        }

        match read_event(&mut client).await {
            SchedulerEvent::WorkflowUpdated { summary } => {
                assert_eq!(summary.status, "queued");
            }
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }
}
