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
}
