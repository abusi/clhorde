//! Long-lived async IPC client between the scheduler and `clhorded`.
//!
//! Mirrors the TUI's connector but drops PTY byte forwarding — the scheduler
//! only consumes structured `DaemonEvent`s. Connect once, get an
//! [`mpsc::UnboundedSender<ClientRequest>`] for outbound traffic and an
//! [`mpsc::UnboundedReceiver<DaemonMessage>`] for inbound events plus a
//! disconnect sentinel.
//!
//! Reconnect is *not* automatic at this layer: the caller is in a better
//! position to decide whether to retry, with what backoff, and whether to
//! preserve any in-flight subscriptions. The `daemon` subcommand wraps this
//! in a reconnect loop in `main.rs`.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use clhorde_core::ipc::{self, MAX_FRAME_SIZE};
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

/// Message yielded by the receive channel.
#[derive(Debug)]
pub enum DaemonMessage {
    /// Structured daemon event.
    Event(Box<DaemonEvent>),
    /// The daemon disconnected, decoded an oversized frame, or sent a
    /// non-deserializable JSON payload. The receiver side is now drained;
    /// the caller should reconnect or shut down.
    Disconnected,
}

/// Connect to the running daemon over its Unix socket.
///
/// Returns a write-side sender and a read-side receiver. Both halves are
/// driven by independent background tasks; the channels close when the
/// daemon disconnects or when the returned senders are dropped.
pub async fn connect() -> Result<
    (
        mpsc::UnboundedSender<ClientRequest>,
        mpsc::UnboundedReceiver<DaemonMessage>,
    ),
    io::Error,
> {
    let socket_path = ipc::daemon_socket_path();
    let stream = tokio::net::UnixStream::connect(&socket_path).await?;
    let (reader, writer) = tokio::io::split(stream);
    Ok(spawn_loops(reader, writer))
}

/// Errors from one-shot CLI flows. Surfaced to the user as the "Is the
/// daemon running?" message in the CLI wrapper.
#[derive(Debug)]
pub enum OneShotError {
    /// Could not connect — daemon not running, socket missing, perms wrong.
    Unreachable(io::Error),
    /// The writer or reader half closed mid-send. Usually means the daemon
    /// crashed while we were talking to it.
    Disconnected,
    /// The daemon never replied with a Pong within the budget. The caller
    /// should treat this as a failure: we don't know whether the daemon
    /// processed the requests.
    Timeout,
}

impl std::fmt::Display for OneShotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OneShotError::Unreachable(e) => {
                write!(f, "cannot reach daemon ({e}). Is it running? Start with: clhorded")
            }
            OneShotError::Disconnected => write!(f, "daemon disconnected mid-send"),
            OneShotError::Timeout => write!(f, "timed out waiting for daemon Pong"),
        }
    }
}

impl std::error::Error for OneShotError {}

/// Default budget for [`send_one_shot`] before declaring [`OneShotError::Timeout`].
pub const ONE_SHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Send a sequence of requests over a fresh, short-lived daemon connection.
///
/// After the requests, a [`ClientRequest::Ping`] is appended. The daemon
/// processes requests in order, so a `Pong` reply guarantees every prior
/// request was received and dequeued. Without this fence we'd race the
/// connection close against the daemon reading our final frame.
pub async fn send_one_shot(
    requests: Vec<ClientRequest>,
) -> Result<(), OneShotError> {
    let (tx, rx) = connect().await.map_err(OneShotError::Unreachable)?;
    drive_one_shot(tx, rx, requests, ONE_SHOT_TIMEOUT).await
}

/// Test hook for [`send_one_shot`] that doesn't open a real Unix socket.
pub async fn drive_one_shot(
    tx: mpsc::UnboundedSender<ClientRequest>,
    mut rx: mpsc::UnboundedReceiver<DaemonMessage>,
    requests: Vec<ClientRequest>,
    timeout: std::time::Duration,
) -> Result<(), OneShotError> {
    for req in requests {
        tx.send(req).map_err(|_| OneShotError::Disconnected)?;
    }
    tx.send(ClientRequest::Ping)
        .map_err(|_| OneShotError::Disconnected)?;

    loop {
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(DaemonMessage::Event(ev))) => match *ev {
                clhorde_core::protocol::DaemonEvent::Pong => return Ok(()),
                _ => continue,
            },
            Ok(Some(DaemonMessage::Disconnected)) | Ok(None) => {
                return Err(OneShotError::Disconnected);
            }
            Err(_) => return Err(OneShotError::Timeout),
        }
    }
}

/// Test-friendly hook: take any AsyncRead/AsyncWrite halves and run the same
/// read/write loops the production path uses. Lets us drive both ends of a
/// `tokio::io::duplex` pair in unit tests.
pub fn spawn_loops<R, W>(
    reader: R,
    writer: W,
) -> (
    mpsc::UnboundedSender<ClientRequest>,
    mpsc::UnboundedReceiver<DaemonMessage>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (req_tx, req_rx) = mpsc::unbounded_channel::<ClientRequest>();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    tokio::spawn(write_loop(writer, req_rx));
    tokio::spawn(read_loop(reader, msg_tx));
    (req_tx, msg_rx)
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut req_rx: mpsc::UnboundedReceiver<ClientRequest>,
) {
    while let Some(req) = req_rx.recv().await {
        let json = match serde_json::to_vec(&req) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "scheduler: failed to serialize ClientRequest");
                continue;
            }
        };
        let frame = ipc::encode_frame(&json);
        if writer.write_all(&frame).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    msg_tx: mpsc::UnboundedSender<DaemonMessage>,
) {
    loop {
        let mut len_buf = [0u8; 4];
        if reader.read_exact(&mut len_buf).await.is_err() {
            let _ = msg_tx.send(DaemonMessage::Disconnected);
            return;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            tracing::warn!(len, "scheduler: oversized frame, disconnecting");
            let _ = msg_tx.send(DaemonMessage::Disconnected);
            return;
        }

        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).await.is_err() {
            let _ = msg_tx.send(DaemonMessage::Disconnected);
            return;
        }

        // PTY frames are uninteresting to the scheduler — drop them.
        if ipc::is_binary_frame(&payload) {
            continue;
        }

        match serde_json::from_slice::<DaemonEvent>(&payload) {
            Ok(event) => {
                if msg_tx.send(DaemonMessage::Event(Box::new(event))).is_err() {
                    // Receiver dropped — caller is shutting down.
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "scheduler: failed to deserialize daemon event");
                // Don't disconnect on a single bad frame — the daemon may
                // ship newer event variants we don't know about.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clhorde_core::protocol::{DaemonState, PROTOCOL_VERSION};
    use tokio::io::duplex;

    /// Push a JSON-encoded DaemonEvent through a frame writer.
    async fn push_event<W: AsyncWrite + Unpin>(writer: &mut W, event: &DaemonEvent) {
        let json = serde_json::to_vec(event).unwrap();
        let frame = ipc::encode_frame(&json);
        writer.write_all(&frame).await.unwrap();
        writer.flush().await.unwrap();
    }

    #[tokio::test]
    async fn read_loop_yields_event_then_disconnects() {
        let (mut server, client) = duplex(8192);
        let (read_half, write_half) = tokio::io::split(client);
        // We don't care about outbound traffic in this test; spawn loops
        // and let write_loop park on its receiver.
        let (_req_tx, mut msg_rx) = spawn_loops(read_half, write_half);

        push_event(
            &mut server,
            &DaemonEvent::MaxWorkersChanged { count: 7 },
        )
        .await;

        match msg_rx.recv().await.expect("event") {
            DaemonMessage::Event(ev) => match *ev {
                DaemonEvent::MaxWorkersChanged { count } => assert_eq!(count, 7),
                other => panic!("expected MaxWorkersChanged, got {other:?}"),
            },
            other => panic!("expected Event, got {other:?}"),
        }

        // Drop the server end → reader should yield Disconnected.
        drop(server);
        match msg_rx.recv().await.expect("disconnect") {
            DaemonMessage::Disconnected => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_loop_serializes_requests_as_frames() {
        // Drive write_loop directly with a Vec<u8> writer — exercises the
        // full encode path without duplex/split scheduling subtleties.
        // `Subscribe` is a unit variant so we sidestep the latent bug where
        // newtype variants like `SetMaxWorkers(usize)` are not serializable
        // under `#[serde(tag = "type")]` (orthogonal to this phase).
        let buf: Vec<u8> = Vec::new();
        let writer = std::io::Cursor::new(buf);
        let (req_tx, req_rx) = mpsc::unbounded_channel::<ClientRequest>();
        req_tx.send(ClientRequest::Subscribe).unwrap();
        // Closing the sender lets write_loop exit cleanly after draining.
        drop(req_tx);

        let writer = write_loop_to_completion(writer, req_rx).await;
        let bytes = writer.into_inner();

        assert!(bytes.len() >= 4);
        let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        let payload = &bytes[4..4 + len];
        let req: ClientRequest = serde_json::from_slice(payload).unwrap();
        assert!(matches!(req, ClientRequest::Subscribe));
    }

    /// Test helper that runs `write_loop` to completion on the given writer,
    /// then returns it. The caller must close the request sender first.
    async fn write_loop_to_completion<W: AsyncWrite + Unpin>(
        mut writer: W,
        mut req_rx: mpsc::UnboundedReceiver<ClientRequest>,
    ) -> W {
        while let Some(req) = req_rx.recv().await {
            let json = serde_json::to_vec(&req).unwrap();
            let frame = ipc::encode_frame(&json);
            writer.write_all(&frame).await.unwrap();
            writer.flush().await.unwrap();
        }
        writer
    }

    #[tokio::test]
    async fn pty_frames_are_dropped_silently() {
        let (mut server, client) = duplex(8192);
        let (read_half, write_half) = tokio::io::split(client);
        let (_req_tx, mut msg_rx) = spawn_loops(read_half, write_half);

        // Send a valid PTY binary frame followed by a JSON event. We expect
        // only the JSON event to surface.
        let pty_frame = ipc::encode_frame(&{
            let mut buf = Vec::with_capacity(1 + 8 + 5);
            buf.push(0x01); // binary marker
            buf.extend_from_slice(&(42u64).to_be_bytes());
            buf.extend_from_slice(b"hello");
            buf
        });
        server.write_all(&pty_frame).await.unwrap();

        push_event(&mut server, &DaemonEvent::Pong).await;

        match msg_rx.recv().await.expect("event") {
            DaemonMessage::Event(ev) => assert!(matches!(*ev, DaemonEvent::Pong)),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_does_not_disconnect() {
        let (mut server, client) = duplex(8192);
        let (read_half, write_half) = tokio::io::split(client);
        let (_req_tx, mut msg_rx) = spawn_loops(read_half, write_half);

        // Garbage JSON frame.
        let bad = ipc::encode_frame(b"{not json}");
        server.write_all(&bad).await.unwrap();
        server.flush().await.unwrap();

        // Recover by sending a valid event — should still come through.
        push_event(&mut server, &DaemonEvent::Pong).await;

        match msg_rx.recv().await.expect("event") {
            DaemonMessage::Event(ev) => assert!(matches!(*ev, DaemonEvent::Pong)),
            other => panic!("expected Event after malformed frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn round_trip_state_snapshot() {
        let (mut server, client) = duplex(16 * 1024);
        let (read_half, write_half) = tokio::io::split(client);
        let (_req_tx, mut msg_rx) = spawn_loops(read_half, write_half);

        let snap = DaemonState {
            prompts: vec![],
            max_workers: 3,
            active_workers: 1,
            default_mode: "interactive".to_string(),
            protocol_version: PROTOCOL_VERSION,
        };
        push_event(&mut server, &DaemonEvent::StateSnapshot(snap)).await;

        match msg_rx.recv().await.expect("event") {
            DaemonMessage::Event(ev) => match *ev {
                DaemonEvent::StateSnapshot(s) => {
                    assert_eq!(s.max_workers, 3);
                    assert_eq!(s.active_workers, 1);
                }
                other => panic!("expected StateSnapshot, got {other:?}"),
            },
            other => panic!("expected Event, got {other:?}"),
        }
    }

    // ── one-shot helper ──

    #[tokio::test]
    async fn one_shot_returns_after_pong() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMessage>();

        // Fake daemon: feed back a Pong as soon as we see the Ping.
        tokio::spawn(async move {
            while let Some(req) = req_rx.recv().await {
                if matches!(req, ClientRequest::Ping) {
                    let _ = msg_tx
                        .send(DaemonMessage::Event(Box::new(DaemonEvent::Pong)));
                    break;
                }
            }
        });

        let result = drive_one_shot(
            req_tx,
            msg_rx,
            vec![ClientRequest::Subscribe],
            std::time::Duration::from_secs(2),
        )
        .await;
        assert!(matches!(result, Ok(())));
    }

    #[tokio::test]
    async fn one_shot_times_out_when_daemon_silent() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel::<ClientRequest>();
        let (_msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMessage>();
        // No fake daemon — Ping never gets a response.
        let result = drive_one_shot(
            req_tx,
            msg_rx,
            vec![ClientRequest::Subscribe],
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(matches!(result, Err(OneShotError::Timeout)));
    }

    #[tokio::test]
    async fn one_shot_reports_disconnect() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel::<ClientRequest>();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMessage>();
        // Drop the daemon side immediately.
        drop(msg_tx);

        let result = drive_one_shot(
            req_tx,
            msg_rx,
            vec![ClientRequest::Subscribe],
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(OneShotError::Disconnected)));
    }

    #[tokio::test]
    async fn one_shot_skips_irrelevant_events_until_pong() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMessage>();

        // Daemon emits a couple of unrelated events before the Pong.
        tokio::spawn(async move {
            while let Some(req) = req_rx.recv().await {
                if matches!(req, ClientRequest::Ping) {
                    let _ = msg_tx.send(DaemonMessage::Event(Box::new(
                        DaemonEvent::MaxWorkersChanged { count: 3 },
                    )));
                    let _ = msg_tx.send(DaemonMessage::Event(Box::new(
                        DaemonEvent::ActiveWorkersChanged { count: 1 },
                    )));
                    let _ = msg_tx
                        .send(DaemonMessage::Event(Box::new(DaemonEvent::Pong)));
                    break;
                }
            }
        });

        let result = drive_one_shot(
            req_tx,
            msg_rx,
            vec![ClientRequest::Subscribe],
            std::time::Duration::from_secs(2),
        )
        .await;
        assert!(matches!(result, Ok(())));
    }
}
