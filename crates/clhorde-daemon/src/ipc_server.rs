use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use tracing::{info, warn};

use clhorde_core::ipc::{self, MAX_FRAME_SIZE, PTY_FRAME_MARKER};
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

/// Command sent from a client handler to the main orchestrator loop.
pub struct ServerCommand {
    pub session_id: usize,
    pub request: ClientRequest,
}

/// Read a length-delimited frame asynchronously.
async fn read_frame_async(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<Vec<u8>, std::io::Error> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Frame too large: {len}"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Write a length-delimited frame asynchronously.
async fn write_frame_async(
    writer: &mut (impl AsyncWriteExt + Unpin),
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Run the IPC server, accepting connections on the given socket path.
pub async fn run_server(
    socket_path: PathBuf,
    cmd_tx: mpsc::UnboundedSender<ServerCommand>,
    session_register_tx: mpsc::UnboundedSender<(usize, mpsc::Sender<DaemonEvent>)>,
    session_unregister_tx: mpsc::UnboundedSender<usize>,
    pty_byte_tx: tokio::sync::broadcast::Sender<(usize, Vec<u8>)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = UnixListener::bind(&socket_path)?;
    info!(socket = %socket_path.display(), "IPC server listening");

    let mut next_session_id: usize = 1;

    loop {
        let (stream, _addr) = listener.accept().await?;
        let session_id = next_session_id;
        next_session_id += 1;

        let cmd_tx = cmd_tx.clone();
        let session_register_tx = session_register_tx.clone();
        let session_unregister_tx = session_unregister_tx.clone();
        let pty_byte_tx = pty_byte_tx.clone();

        tokio::spawn(async move {
            handle_client(
                stream,
                session_id,
                cmd_tx,
                session_register_tx,
                session_unregister_tx,
                pty_byte_tx,
            )
            .await;
        });
    }
}

async fn handle_client(
    stream: UnixStream,
    session_id: usize,
    cmd_tx: mpsc::UnboundedSender<ServerCommand>,
    session_register_tx: mpsc::UnboundedSender<(usize, mpsc::Sender<DaemonEvent>)>,
    session_unregister_tx: mpsc::UnboundedSender<usize>,
    pty_byte_tx: tokio::sync::broadcast::Sender<(usize, Vec<u8>)>,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Register this client's event channel with the orchestrator
    let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(1024);
    if session_register_tx.send((session_id, event_tx)).is_err() {
        return;
    }

    let write_loop = async {
        // PTY byte forwarding is gated on subscription.
        // We start with a receiver but only select! on it when pty_active is true.
        let mut pty_active = false;
        let mut pty_byte_rx = pty_byte_tx.subscribe();

        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else { break; };

                    // Toggle PTY forwarding based on Subscribed/Unsubscribed events
                    match &event {
                        DaemonEvent::Subscribed => {
                            pty_active = true;
                            // Create a fresh receiver to avoid stale buffered bytes
                            pty_byte_rx = pty_byte_tx.subscribe();
                        }
                        DaemonEvent::Unsubscribed => {
                            pty_active = false;
                        }
                        _ => {}
                    }

                    let json = match serde_json::to_vec(&event) {
                        Ok(j) => j,
                        Err(e) => {
                            warn!(session_id, error = %e, "failed to serialize event");
                            continue;
                        }
                    };
                    if write_frame_async(&mut writer, &json).await.is_err() {
                        break;
                    }
                }
                pty_result = pty_byte_rx.recv(), if pty_active => {
                    match pty_result {
                        Ok((prompt_id, bytes)) => {
                            let frame = ipc::encode_pty_frame(prompt_id, &bytes);
                            if write_frame_async(&mut writer, &frame).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Dropped some PTY frames — not fatal
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    };

    let read_loop = async {
        loop {
            let payload = match read_frame_async(&mut reader).await {
                Ok(p) => p,
                Err(_) => break,
            };

            // Skip binary PTY frames from client (shouldn't happen, but be safe)
            if !payload.is_empty() && payload[0] == PTY_FRAME_MARKER {
                continue;
            }

            let request: ClientRequest = match serde_json::from_slice(&payload) {
                Ok(r) => r,
                Err(e) => {
                    warn!(session_id, error = %e, "invalid request from client");
                    continue;
                }
            };

            // Check for shutdown
            let is_shutdown = matches!(request, ClientRequest::Shutdown);

            if cmd_tx
                .send(ServerCommand {
                    session_id,
                    request,
                })
                .is_err()
            {
                break;
            }

            if is_shutdown {
                break;
            }
        }
    };

    // Run both loops concurrently; when either finishes, clean up
    tokio::select! {
        _ = write_loop => {},
        _ = read_loop => {},
    }

    let _ = session_unregister_tx.send(session_id);
}
