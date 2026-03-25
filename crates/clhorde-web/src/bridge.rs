//! Async IPC client that bridges HTTP requests to the daemon.

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use clhorde_core::ipc::{self, MAX_FRAME_SIZE};
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

/// Bridge to the daemon via Unix domain socket.
///
/// Each request opens a fresh connection (like the CLI client) to avoid
/// multiplexing complexity. The socket path is stored for reuse.
pub struct DaemonBridge {
    socket_path: PathBuf,
    /// Serializes concurrent requests to avoid interleaved frames on a
    /// shared connection. Each request gets its own connection, but this
    /// mutex prevents thundering-herd on the socket file.
    _lock: Mutex<()>,
}

impl DaemonBridge {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            _lock: Mutex::new(()),
        }
    }

    /// Send a request and wait for the first JSON response event.
    pub async fn request(&self, req: ClientRequest) -> Result<DaemonEvent, BridgeError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| BridgeError::Connect(e.to_string()))?;

        // Send request frame
        let json = serde_json::to_vec(&req)
            .map_err(|e| BridgeError::Protocol(format!("serialize: {e}")))?;
        let frame = ipc::encode_frame(&json);
        stream
            .write_all(&frame)
            .await
            .map_err(|e| BridgeError::Io(e.to_string()))?;
        stream
            .flush()
            .await
            .map_err(|e| BridgeError::Io(e.to_string()))?;

        // Read response frames, skipping binary PTY frames
        loop {
            let mut len_buf = [0u8; 4];
            stream
                .read_exact(&mut len_buf)
                .await
                .map_err(|e| BridgeError::Io(format!("read length: {e}")))?;

            let len = u32::from_be_bytes(len_buf) as usize;
            if len > MAX_FRAME_SIZE {
                return Err(BridgeError::Protocol(format!(
                    "frame too large: {len} bytes"
                )));
            }

            let mut payload = vec![0u8; len];
            stream
                .read_exact(&mut payload)
                .await
                .map_err(|e| BridgeError::Io(format!("read payload: {e}")))?;

            // Skip binary PTY frames
            if ipc::is_binary_frame(&payload) {
                continue;
            }

            let event: DaemonEvent = serde_json::from_slice(&payload)
                .map_err(|e| BridgeError::Protocol(format!("deserialize: {e}")))?;
            return Ok(event);
        }
    }
}

#[derive(Debug)]
pub enum BridgeError {
    /// Cannot connect to daemon socket.
    Connect(String),
    /// I/O error during communication.
    Io(String),
    /// Protocol-level error (serialization, frame size, etc.).
    Protocol(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Connect(e) => write!(f, "daemon connection failed: {e}"),
            BridgeError::Io(e) => write!(f, "daemon I/O error: {e}"),
            BridgeError::Protocol(e) => write!(f, "protocol error: {e}"),
        }
    }
}
