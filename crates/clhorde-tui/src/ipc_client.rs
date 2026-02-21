use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use clhorde_core::ipc;
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

/// Message received from the daemon.
pub enum DaemonMessage {
    /// A JSON event from the daemon.
    Event(Box<DaemonEvent>),
    /// Binary PTY bytes for a specific prompt.
    PtyBytes { prompt_id: usize, data: Vec<u8> },
    /// The connection to the daemon was lost.
    Disconnected,
}

/// Connect to the daemon and return (sender, receiver) channels.
///
/// The sender is used to send `ClientRequest` messages to the daemon.
/// The receiver yields `DaemonMessage` variants (events, PTY bytes, disconnection).
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

    let (req_tx, req_rx) = mpsc::unbounded_channel::<ClientRequest>();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<DaemonMessage>();

    // Writer task: serialize ClientRequest to JSON, send as length-delimited frames
    tokio::spawn(write_loop(writer, req_rx));

    // Reader task: read frames, dispatch JSON events vs binary PTY bytes
    tokio::spawn(read_loop(reader, msg_tx));

    Ok((req_tx, msg_rx))
}

async fn write_loop(
    mut writer: tokio::io::WriteHalf<tokio::net::UnixStream>,
    mut req_rx: mpsc::UnboundedReceiver<ClientRequest>,
) {
    while let Some(req) = req_rx.recv().await {
        let json = match serde_json::to_vec(&req) {
            Ok(j) => j,
            Err(_) => continue,
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

async fn read_loop(
    mut reader: tokio::io::ReadHalf<tokio::net::UnixStream>,
    msg_tx: mpsc::UnboundedSender<DaemonMessage>,
) {
    loop {
        // Read 4-byte length header
        let mut len_buf = [0u8; 4];
        if reader.read_exact(&mut len_buf).await.is_err() {
            let _ = msg_tx.send(DaemonMessage::Disconnected);
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            let _ = msg_tx.send(DaemonMessage::Disconnected);
            break;
        }

        // Read payload
        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).await.is_err() {
            let _ = msg_tx.send(DaemonMessage::Disconnected);
            break;
        }

        // Dispatch: binary PTY frame vs JSON event
        if ipc::is_binary_frame(&payload) {
            match ipc::decode_pty_frame(&payload) {
                Ok((prompt_id, data)) => {
                    if msg_tx
                        .send(DaemonMessage::PtyBytes { prompt_id, data })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => continue,
            }
        } else {
            match serde_json::from_slice::<DaemonEvent>(&payload) {
                Ok(event) => {
                    if msg_tx.send(DaemonMessage::Event(Box::new(event))).is_err() {
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
    }
}
