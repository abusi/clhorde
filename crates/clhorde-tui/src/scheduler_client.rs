//! One-shot client for the scheduler control socket.
//!
//! The TUI doesn't keep an open connection to the scheduler — Drafts
//! and Workflows views poll on a tick. Each call here:
//!   1. Connects to `~/.local/share/clhorde/scheduler.sock`,
//!   2. Writes one length-delimited [`ControlRequest`],
//!   3. Reads back one [`ControlResponse`] within a small timeout,
//!   4. Drops the connection.
//!
//! Mirrors `clhorde_scheduler::control::client::request` but lives
//! locally so the TUI doesn't need to depend on the scheduler crate
//! (and pull in tera/notify/etc just to deserialize a wire enum).

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use clhorde_core::control::{ControlRequest, ControlResponse, SchedulerEvent};
use clhorde_core::ipc::{self, scheduler_socket_path, MAX_FRAME_SIZE};
use tokio::sync::mpsc;

/// Default per-request budget. Big enough to absorb a single scheduler
/// reconcile pass, small enough that the UI tick never feels frozen.
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug)]
pub enum SchedulerError {
    /// Could not connect — scheduler not running, socket missing,
    /// permissions wrong.
    Unreachable(io::Error),
    /// I/O error after we got a connection.
    Io(io::Error),
    /// The scheduler sent a frame we couldn't decode.
    BadResponse(String),
    /// No response within [`REQUEST_TIMEOUT`].
    Timeout,
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerError::Unreachable(e) => {
                write!(f, "scheduler not reachable ({e})")
            }
            SchedulerError::Io(e) => write!(f, "io: {e}"),
            SchedulerError::BadResponse(s) => write!(f, "bad response: {s}"),
            SchedulerError::Timeout => f.write_str("scheduler did not respond in time"),
        }
    }
}

impl std::error::Error for SchedulerError {}

/// Send `req` to the running scheduler at the default socket path.
pub async fn request(req: ControlRequest) -> Result<ControlResponse, SchedulerError> {
    request_at(scheduler_socket_path(), req).await
}

/// Variant that targets an explicit socket path (used by tests).
pub async fn request_at(
    path: PathBuf,
    req: ControlRequest,
) -> Result<ControlResponse, SchedulerError> {
    match tokio::time::timeout(REQUEST_TIMEOUT, drive(path, req)).await {
        Ok(res) => res,
        Err(_) => Err(SchedulerError::Timeout),
    }
}

/// Notification sent by the long-lived [`subscribe`] task to its
/// owner. Owners typically forward `Event` straight into
/// [`crate::app::App::apply_scheduler_event`] and react to
/// [`SubscriptionMessage::Disconnected`] by spawning a fresh
/// subscribe task after a backoff.
///
/// `Event` carries a [`SchedulerEvent`] which now (post-5.3) includes
/// the `DetailSnapshot`/`DetailUpdated` variants that hold a full
/// [`clhorde_core::control::WorkflowDetail`] — that's the dominant
/// variant in size, while `Disconnected` is small. Boxing the wire
/// type would force a heap allocation per event for no observable
/// gain (we move it once across an unbounded channel and pattern
/// match it into App state).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SubscriptionMessage {
    Event(SchedulerEvent),
    /// The connection died (network error, scheduler stopped, etc.).
    /// Carries the underlying error for diagnostics.
    Disconnected(SchedulerError),
}

/// Spawn a long-lived subscribe task against the scheduler control
/// socket at the default path.
///
/// Sends one [`ControlRequest::Subscribe`] frame, then forwards every
/// [`ControlResponse::Event`] frame as a [`SubscriptionMessage::Event`]
/// on the returned receiver. Emits a single
/// [`SubscriptionMessage::Disconnected`] before dropping the channel
/// so callers can implement reconnect.
///
/// Caller drops the receiver to signal the task to shut down — the
/// task notices via the channel-closed branch and exits.
pub fn subscribe() -> mpsc::UnboundedReceiver<SubscriptionMessage> {
    subscribe_at(scheduler_socket_path())
}

/// Variant of [`subscribe`] that targets an explicit socket path.
/// Used by tests against an in-memory `UnixListener`.
pub fn subscribe_at(path: PathBuf) -> mpsc::UnboundedReceiver<SubscriptionMessage> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let err = match drive_subscribe(path, tx.clone()).await {
            Ok(()) => SchedulerError::Io(io::Error::other("connection closed")),
            Err(e) => e,
        };
        let _ = tx.send(SubscriptionMessage::Disconnected(err));
    });
    rx
}

async fn drive_subscribe(
    path: PathBuf,
    tx: mpsc::UnboundedSender<SubscriptionMessage>,
) -> Result<(), SchedulerError> {
    let stream = UnixStream::connect(&path)
        .await
        .map_err(SchedulerError::Unreachable)?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Send one Subscribe frame.
    let json = serde_json::to_vec(&ControlRequest::Subscribe)
        .map_err(|e| SchedulerError::BadResponse(format!("encode: {e}")))?;
    let frame = ipc::encode_frame(&json);
    writer.write_all(&frame).await.map_err(SchedulerError::Io)?;
    writer.flush().await.map_err(SchedulerError::Io)?;

    // Read frames until the socket dies or the receiver hangs up.
    loop {
        let mut len_buf = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut len_buf).await {
            return Err(SchedulerError::Io(e));
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(SchedulerError::BadResponse(format!(
                "oversized frame: {len}"
            )));
        }
        let mut payload = vec![0u8; len];
        if let Err(e) = reader.read_exact(&mut payload).await {
            return Err(SchedulerError::Io(e));
        }
        let response: ControlResponse = serde_json::from_slice(&payload)
            .map_err(|e| SchedulerError::BadResponse(format!("decode: {e}")))?;
        match response {
            ControlResponse::Event { event } => {
                if tx.send(SubscriptionMessage::Event(event)).is_err() {
                    // Owner dropped the receiver — nothing more to do.
                    return Ok(());
                }
            }
            other => {
                // The server shouldn't speak any other variant on a
                // subscribe connection. Log via a BadResponse error
                // and tear down — the owner will reconnect.
                return Err(SchedulerError::BadResponse(format!(
                    "unexpected non-event frame on subscribe: {other:?}"
                )));
            }
        }
    }
}

async fn drive(
    path: PathBuf,
    req: ControlRequest,
) -> Result<ControlResponse, SchedulerError> {
    let stream = UnixStream::connect(&path)
        .await
        .map_err(SchedulerError::Unreachable)?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    let json = serde_json::to_vec(&req)
        .map_err(|e| SchedulerError::BadResponse(format!("encode: {e}")))?;
    let frame = ipc::encode_frame(&json);
    writer.write_all(&frame).await.map_err(SchedulerError::Io)?;
    writer.flush().await.map_err(SchedulerError::Io)?;

    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(SchedulerError::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(SchedulerError::BadResponse(format!(
            "oversized frame: {len}"
        )));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(SchedulerError::Io)?;
    serde_json::from_slice(&payload)
        .map_err(|e| SchedulerError::BadResponse(format!("decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    /// Minimal echo server for tests: listens on `path`, accepts one
    /// connection, reads one request frame, and writes back the
    /// response that the test handed in.
    async fn spawn_one_shot_server(path: PathBuf, response: ControlResponse) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = tokio::io::split(stream);

            // Read one frame so the client doesn't choke on a closed
            // socket while it's still writing.
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

    #[tokio::test]
    async fn ping_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        spawn_one_shot_server(path.clone(), ControlResponse::Pong).await;

        let resp = request_at(path, ControlRequest::Ping).await.unwrap();
        assert!(matches!(resp, ControlResponse::Pong));
    }

    #[tokio::test]
    async fn status_response_decodes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        let response = ControlResponse::Status {
            workflows: vec![clhorde_core::control::WorkflowSummary {
                name: "x".into(),
                status: "queued".into(),
                failure_reason: None,
                priority: 3,
                queued_at: None,
                started_at: None,
                finished_at: None,
                prompt_ids: vec![],
            }],
            root: None,
        };
        spawn_one_shot_server(path.clone(), response.clone()).await;

        let got = request_at(path, ControlRequest::Status { name: None })
            .await
            .unwrap();
        assert_eq!(got, response);
    }

    #[tokio::test]
    async fn missing_socket_yields_unreachable() {
        let tmp = TempDir::new().unwrap();
        let phantom = tmp.path().join("not-here.sock");
        let err = request_at(phantom, ControlRequest::Ping)
            .await
            .unwrap_err();
        assert!(matches!(err, SchedulerError::Unreachable(_)));
    }

    /// Spawn a minimal subscribe-mode server: accepts one connection,
    /// reads one Subscribe frame, then writes the supplied event
    /// frames in order. Closes the socket after the last write so the
    /// client gets a clean disconnect.
    async fn spawn_subscribe_server(path: PathBuf, events: Vec<SchedulerEvent>) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = tokio::io::split(stream);

            // Drain the Subscribe request frame.
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
            // Drop the writer half — the client sees EOF and reports
            // SubscriptionMessage::Disconnected.
        });
    }

    #[tokio::test]
    async fn subscribe_forwards_events_until_disconnect() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sched.sock");
        let summary = clhorde_core::control::WorkflowSummary {
            name: "x".into(),
            status: "queued".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec![],
        };
        let events = vec![
            SchedulerEvent::Snapshot {
                workflows: vec![summary.clone()],
                root: Some("/tmp/repo".into()),
            },
            SchedulerEvent::WorkflowUpdated {
                summary: clhorde_core::control::WorkflowSummary {
                    status: "implementing".into(),
                    ..summary
                },
            },
        ];
        spawn_subscribe_server(path.clone(), events).await;

        let mut rx = subscribe_at(path);
        let first = rx.recv().await.expect("first event");
        match first {
            SubscriptionMessage::Event(SchedulerEvent::Snapshot {
                workflows, root,
            }) => {
                assert_eq!(workflows.len(), 1);
                assert_eq!(workflows[0].name, "x");
                assert_eq!(root.as_deref(), Some("/tmp/repo"));
            }
            other => panic!("expected Snapshot event, got {other:?}"),
        }
        let second = rx.recv().await.expect("second event");
        match second {
            SubscriptionMessage::Event(SchedulerEvent::WorkflowUpdated {
                summary,
            }) => {
                assert_eq!(summary.status, "implementing");
            }
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }
        // Server closes the socket after the second frame → we expect
        // Disconnected next.
        let third = rx.recv().await.expect("disconnect message");
        match third {
            SubscriptionMessage::Disconnected(_) => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_reports_disconnect_when_socket_missing() {
        // No server bound at this path — subscribe_at should still
        // return an mpsc receiver and emit a single Disconnected
        // before it closes.
        let tmp = TempDir::new().unwrap();
        let phantom = tmp.path().join("missing.sock");
        let mut rx = subscribe_at(phantom);
        let msg = rx.recv().await.expect("disconnect message");
        match msg {
            SubscriptionMessage::Disconnected(SchedulerError::Unreachable(_)) => {}
            other => panic!("expected Unreachable disconnect, got {other:?}"),
        }
        // Channel closes after the disconnect frame.
        assert!(rx.recv().await.is_none());
    }
}
