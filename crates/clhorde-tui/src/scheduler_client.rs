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

use clhorde_core::control::{ControlRequest, ControlResponse};
use clhorde_core::ipc::{self, scheduler_socket_path, MAX_FRAME_SIZE};

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
}
