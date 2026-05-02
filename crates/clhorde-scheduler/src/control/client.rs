//! Client helper for the scheduler control socket.
//!
//! Mirrors `daemon_client::send_one_shot` but for the much simpler
//! request/response shape of the control socket: connect, send one
//! request, read one response, drop the connection. Callers that need
//! to issue a sequence of requests on a single connection use
//! [`request_many`].

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use clhorde_core::ipc::{self, scheduler_socket_path, MAX_FRAME_SIZE};

use super::protocol::{ControlRequest, ControlResponse};

/// Default budget for [`request`] before timing out.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// What can go wrong issuing a control-socket request.
#[derive(Debug)]
pub enum ControlError {
    /// Could not connect — scheduler not running, socket missing,
    /// permissions wrong.
    Unreachable(io::Error),
    /// I/O error after we got a connection (typically: scheduler died
    /// mid-exchange).
    Io(io::Error),
    /// The scheduler sent a frame we couldn't decode.
    BadResponse(String),
    /// No response within [`REQUEST_TIMEOUT`].
    Timeout,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::Unreachable(e) => write!(
                f,
                "cannot reach scheduler ({e}). Is it running? \
                 Start with: clhorde-scheduler daemon"
            ),
            ControlError::Io(e) => write!(f, "io: {e}"),
            ControlError::BadResponse(s) => write!(f, "bad response: {s}"),
            ControlError::Timeout => f.write_str("scheduler did not respond in time"),
        }
    }
}

impl std::error::Error for ControlError {}

/// Connect, send `req`, read one response, return.
pub async fn request(req: ControlRequest) -> Result<ControlResponse, ControlError> {
    let path = scheduler_socket_path();
    request_at(path, req).await
}

/// Variant of [`request`] that targets an explicit socket path. Useful
/// for tests and for redirecting to a non-default location.
pub async fn request_at(
    path: PathBuf,
    req: ControlRequest,
) -> Result<ControlResponse, ControlError> {
    let stream = UnixStream::connect(&path)
        .await
        .map_err(ControlError::Unreachable)?;
    let mut responses = drive(stream, vec![req]).await?;
    responses.pop().ok_or_else(|| {
        ControlError::BadResponse("scheduler closed before responding".into())
    })
}

/// Send a sequence of requests on one connection and collect every
/// response in order. Used by the TUI when it wants to fetch state and
/// then issue a follow-up mutation atomically.
pub async fn request_many_at(
    path: PathBuf,
    requests: Vec<ControlRequest>,
) -> Result<Vec<ControlResponse>, ControlError> {
    let stream = UnixStream::connect(&path)
        .await
        .map_err(ControlError::Unreachable)?;
    drive(stream, requests).await
}

async fn drive(
    stream: UnixStream,
    requests: Vec<ControlRequest>,
) -> Result<Vec<ControlResponse>, ControlError> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Send every request first, then read responses in order. The server
    // processes them sequentially per connection (we don't pipeline at
    // the protocol level; this is just convenience).
    for req in &requests {
        let json = serde_json::to_vec(req)
            .map_err(|e| ControlError::BadResponse(format!("encode: {e}")))?;
        let frame = ipc::encode_frame(&json);
        writer.write_all(&frame).await.map_err(ControlError::Io)?;
    }
    writer.flush().await.map_err(ControlError::Io)?;

    let mut out = Vec::with_capacity(requests.len());
    for _ in 0..requests.len() {
        match tokio::time::timeout(REQUEST_TIMEOUT, read_one(&mut reader)).await {
            Ok(Ok(resp)) => out.push(resp),
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(ControlError::Timeout),
        }
    }
    Ok(out)
}

async fn read_one<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<ControlResponse, ControlError> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(ControlError::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(ControlError::BadResponse(format!(
            "oversized response frame: {len}"
        )));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(ControlError::Io)?;
    serde_json::from_slice(&payload)
        .map_err(|e| ControlError::BadResponse(format!("decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::server;
    use crate::orchestrator::Orchestrator;
    use crate::persistence::WorkflowStore;
    use clhorde_core::protocol::ClientRequest;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    /// Bind a real UnixListener under tmp + spawn the server on it, return
    /// the socket path.
    async fn spawn_server_at(tmp: &TempDir) -> PathBuf {
        let socket = tmp.path().join("scheduler.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let store = WorkflowStore::open(tmp.path().join("store"));
        let (tx, _rx) = mpsc::unbounded_channel::<ClientRequest>();
        let orch = Arc::new(Mutex::new(Orchestrator::new(tmp.path(), store, tx)));
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let orch = orch.clone();
                tokio::spawn(async move {
                    let (sr, sw) = tokio::io::split(stream);
                    let _ = server::run_with_streams(sr, sw, orch).await;
                });
            }
        });
        socket
    }

    #[tokio::test]
    async fn request_at_round_trips_ping() {
        let tmp = TempDir::new().unwrap();
        let socket = spawn_server_at(&tmp).await;

        let resp = request_at(socket, ControlRequest::Ping).await.unwrap();
        assert!(matches!(resp, ControlResponse::Pong));
    }

    #[tokio::test]
    async fn request_at_returns_unreachable_when_server_missing() {
        let tmp = TempDir::new().unwrap();
        let phantom = tmp.path().join("not-here.sock");
        let err = request_at(phantom, ControlRequest::Ping).await.unwrap_err();
        match err {
            ControlError::Unreachable(_) => {}
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_many_preserves_order() {
        let tmp = TempDir::new().unwrap();
        let socket = spawn_server_at(&tmp).await;

        let resps = request_many_at(
            socket,
            vec![
                ControlRequest::Ping,
                ControlRequest::Status { name: None },
                ControlRequest::Ping,
            ],
        )
        .await
        .unwrap();
        assert_eq!(resps.len(), 3);
        assert!(matches!(resps[0], ControlResponse::Pong));
        assert!(matches!(resps[1], ControlResponse::Status { .. }));
        assert!(matches!(resps[2], ControlResponse::Pong));
    }
}
