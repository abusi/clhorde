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
use tracing::{debug, info, warn};

use super::protocol::{ControlRequest, ControlResponse};
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

        let response = match serde_json::from_slice::<ControlRequest>(&payload) {
            Ok(req) => {
                let mut guard = orch
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                dispatch_request(&mut guard, req)
            }
            Err(e) => ControlResponse::Error {
                message: format!("malformed request: {e}"),
            },
        };

        let json = serde_json::to_vec(&response).unwrap_or_else(|e| {
            // serde_json on our owned types should never fail; if it
            // does, send a minimal error to avoid hanging the client.
            format!(r#"{{"type":"error","message":"serialize: {e}"}}"#).into_bytes()
        });
        let frame = ipc::encode_frame(&json);
        writer.write_all(&frame).await?;
        writer.flush().await?;
    }
}

/// Apply one request to the orchestrator and return the response.
/// Pure dispatch — no I/O, no awaits — so tests can hit it directly.
pub fn dispatch_request(
    orch: &mut Orchestrator,
    req: ControlRequest,
) -> ControlResponse {
    match req {
        ControlRequest::Ping => ControlResponse::Pong,
        ControlRequest::Status { name: None } => ControlResponse::Status {
            workflows: orch.summaries(),
        },
        ControlRequest::Status { name: Some(n) } => match orch.summary(&n) {
            Some(s) => ControlResponse::Status {
                workflows: vec![s],
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
            ControlResponse::Status { workflows } => assert!(workflows.is_empty()),
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
            ControlResponse::Status { workflows } => {
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
            ControlResponse::Status { workflows } => {
                assert_eq!(workflows.len(), 1);
            }
            other => panic!("expected Status, got {other:?}"),
        }

        drop(client);
        let _ = server_task.await;
    }
}
