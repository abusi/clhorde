//! Lightweight async IPC client for one-shot CLI commands.
//!
//! Unlike the TUI's long-lived `ipc_client.rs`, the CLI connects,
//! sends one request, reads one response, and disconnects.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use clhorde_core::ipc;
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

const CONNECT_ERROR: &str = "Failed to connect to daemon. Is it running? Start with: clhorded";

/// Connect to daemon, send a request, wait for first JSON response event.
pub async fn request(req: ClientRequest) -> Result<DaemonEvent, String> {
    let socket_path = ipc::daemon_socket_path();
    let mut stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .map_err(|_| CONNECT_ERROR.to_string())?;

    // Send request
    let json = serde_json::to_vec(&req).map_err(|e| format!("Serialize error: {e}"))?;
    let frame = ipc::encode_frame(&json);
    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("Write error: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("Flush error: {e}"))?;

    // Read one response frame
    loop {
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("Read error: {e}"))?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            return Err("Response frame too large".to_string());
        }
        let mut payload = vec![0u8; len];
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("Read error: {e}"))?;

        // Skip binary PTY frames, wait for JSON
        if ipc::is_binary_frame(&payload) {
            continue;
        }

        let event: DaemonEvent =
            serde_json::from_slice(&payload).map_err(|e| format!("Deserialize error: {e}"))?;
        return Ok(event);
    }
}

/// Connect to daemon, subscribe, and stream events via callbacks.
/// `on_event` returns `false` to stop. `on_pty` returns `false` to stop.
pub async fn stream_events(
    setup_requests: Vec<ClientRequest>,
    mut on_event: impl FnMut(DaemonEvent) -> bool,
    mut on_pty: impl FnMut(usize, &[u8]) -> bool,
) -> Result<(), String> {
    let socket_path = ipc::daemon_socket_path();
    let mut stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .map_err(|_| CONNECT_ERROR.to_string())?;

    // Send all setup requests
    for req in setup_requests {
        let json = serde_json::to_vec(&req).map_err(|e| format!("Serialize error: {e}"))?;
        let frame = ipc::encode_frame(&json);
        stream
            .write_all(&frame)
            .await
            .map_err(|e| format!("Write error: {e}"))?;
    }
    stream
        .flush()
        .await
        .map_err(|e| format!("Flush error: {e}"))?;

    // Read event stream
    loop {
        let mut len_buf = [0u8; 4];
        if stream.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            break;
        }
        let mut payload = vec![0u8; len];
        if stream.read_exact(&mut payload).await.is_err() {
            break;
        }

        if ipc::is_binary_frame(&payload) {
            if let Ok((prompt_id, data)) = ipc::decode_pty_frame(&payload) {
                if !on_pty(prompt_id, &data) {
                    break;
                }
            }
        } else if let Ok(event) = serde_json::from_slice::<DaemonEvent>(&payload) {
            if !on_event(event) {
                break;
            }
        }
    }

    Ok(())
}
