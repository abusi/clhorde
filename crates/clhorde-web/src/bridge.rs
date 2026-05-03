//! Async IPC client that bridges HTTP/WebSocket to the daemon Unix socket.
//!
//! `DaemonBridge` maintains a persistent connection to the daemon, multiplexing
//! request-response calls with a broadcast stream of `DaemonEvent`s and PTY bytes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use clhorde_core::ipc::{self, MAX_FRAME_SIZE};
use clhorde_core::protocol::{ClientRequest, DaemonEvent};

/// Maximum pending request-response pairs.
const REQUEST_CHANNEL_SIZE: usize = 64;

/// Broadcast channel capacity for events.
const EVENT_BROADCAST_SIZE: usize = 256;

/// PTY bytes broadcast channel capacity.
const PTY_BROADCAST_SIZE: usize = 512;

/// Maximum reconnection backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Initial reconnection backoff.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// A pending request waiting for its response.
struct PendingRequest {
    request: ClientRequest,
    response_tx: oneshot::Sender<Result<DaemonEvent, BridgeError>>,
}

/// PTY bytes received from the daemon.
#[derive(Debug, Clone)]
pub struct PtyFrame {
    pub prompt_id: usize,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum BridgeError {
    /// Connection was lost while waiting for a response.
    Disconnected,
    /// Failed to serialize the request.
    Serialize(String),
    /// The send channel is full or closed.
    SendFailed,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Disconnected => write!(f, "disconnected from daemon"),
            BridgeError::Serialize(e) => write!(f, "serialize error: {e}"),
            BridgeError::SendFailed => write!(f, "failed to send request"),
        }
    }
}

impl std::error::Error for BridgeError {}

/// Async bridge to the clhorde daemon over Unix socket IPC.
///
/// Provides request-response calls and broadcast streams for events and PTY bytes.
/// Automatically reconnects with exponential backoff when the daemon disconnects.
pub struct DaemonBridge {
    socket_path: PathBuf,
    connected: AtomicBool,
    /// Channel to submit requests into the write loop.
    request_tx: mpsc::Sender<PendingRequest>,
    /// Broadcast sender for daemon events (JSON frames).
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Broadcast sender for PTY binary frames.
    pty_tx: broadcast::Sender<PtyFrame>,
    /// Handle to the connection manager task, kept alive for the bridge lifetime.
    _conn_handle: Mutex<tokio::task::JoinHandle<()>>,
}

impl DaemonBridge {
    /// Connect to the daemon and spawn the background connection manager.
    pub async fn connect(socket_path: PathBuf) -> Result<Arc<Self>, BridgeError> {
        let (request_tx, request_rx) = mpsc::channel::<PendingRequest>(REQUEST_CHANNEL_SIZE);
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(EVENT_BROADCAST_SIZE);
        let (pty_tx, _) = broadcast::channel::<PtyFrame>(PTY_BROADCAST_SIZE);
        let connected = AtomicBool::new(false);

        let bridge = Arc::new(Self {
            socket_path,
            connected,
            request_tx,
            event_tx,
            pty_tx,
            _conn_handle: Mutex::new(tokio::spawn(async {})), // placeholder
        });

        // Spawn the connection manager that handles connect/reconnect.
        let handle = tokio::spawn(connection_manager(
            bridge.socket_path.clone(),
            bridge.event_tx.clone(),
            bridge.pty_tx.clone(),
            request_rx,
            bridge.clone(),
        ));

        *bridge._conn_handle.lock().await = handle;

        // Wait briefly for the initial connection to establish.
        for _ in 0..20 {
            if bridge.connected.load(Ordering::Relaxed) {
                return Ok(bridge);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Return the bridge even if not yet connected — it will reconnect in the background.
        warn!("Initial connection not yet established, bridge will reconnect in background");
        Ok(bridge)
    }

    /// Whether the bridge is currently connected to the daemon.
    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Send a request to the daemon and wait for the response.
    pub async fn send_request(&self, request: ClientRequest) -> Result<DaemonEvent, BridgeError> {
        let (response_tx, response_rx) = oneshot::channel();

        self.request_tx
            .send(PendingRequest {
                request,
                response_tx,
            })
            .await
            .map_err(|_| BridgeError::SendFailed)?;

        response_rx.await.map_err(|_| BridgeError::Disconnected)?
    }

    /// Subscribe to the daemon event broadcast stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    /// Subscribe to the PTY bytes broadcast stream.
    pub fn subscribe_pty(&self) -> broadcast::Receiver<PtyFrame> {
        self.pty_tx.subscribe()
    }
}

/// Background task that manages the socket connection lifecycle.
///
/// Connects to the daemon, runs read/write loops, and reconnects with
/// exponential backoff on disconnection.
async fn connection_manager(
    socket_path: PathBuf,
    event_tx: broadcast::Sender<DaemonEvent>,
    pty_tx: broadcast::Sender<PtyFrame>,
    mut request_rx: mpsc::Receiver<PendingRequest>,
    bridge: Arc<DaemonBridge>,
) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        info!(path = %socket_path.display(), "connecting to daemon");

        match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(stream) => {
                info!("connected to daemon");
                bridge.connected.store(true, Ordering::Relaxed);
                backoff = INITIAL_BACKOFF;

                run_connection(stream, &event_tx, &pty_tx, &mut request_rx, &bridge).await;

                bridge.connected.store(false, Ordering::Relaxed);
                warn!("daemon connection lost");
            }
            Err(e) => {
                debug!(error = %e, "failed to connect to daemon");
            }
        }

        // Drain and fail any pending requests before reconnecting.
        drain_pending_requests(&mut request_rx).await;

        info!(backoff_ms = backoff.as_millis(), "reconnecting after backoff");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Run the read/write loops for a single connection. Returns when the connection drops.
async fn run_connection(
    stream: tokio::net::UnixStream,
    event_tx: &broadcast::Sender<DaemonEvent>,
    pty_tx: &broadcast::Sender<PtyFrame>,
    request_rx: &mut mpsc::Receiver<PendingRequest>,
    bridge: &Arc<DaemonBridge>,
) {
    let (reader, writer) = tokio::io::split(stream);

    // Channel for the read loop to deliver responses to pending requests.
    let (response_tx, mut response_rx) = mpsc::unbounded_channel::<DaemonEvent>();

    // Spawn the read loop.
    let event_tx2 = event_tx.clone();
    let pty_tx2 = pty_tx.clone();
    let read_handle = tokio::spawn(async move {
        read_loop(reader, event_tx2, pty_tx2, response_tx).await;
    });

    // Write loop: process pending requests, forwarding them to the socket writer.
    let write_result =
        write_loop(writer, request_rx, &mut response_rx, bridge).await;

    // If write loop exited, abort the read loop.
    read_handle.abort();
    let _ = read_handle.await;

    if let Err(e) = write_result {
        debug!(error = %e, "write loop ended");
    }
}

/// Read frames from the daemon, dispatching JSON events and PTY bytes.
async fn read_loop(
    mut reader: tokio::io::ReadHalf<tokio::net::UnixStream>,
    event_tx: broadcast::Sender<DaemonEvent>,
    pty_tx: broadcast::Sender<PtyFrame>,
    response_tx: mpsc::UnboundedSender<DaemonEvent>,
) {
    loop {
        let mut len_buf = [0u8; 4];
        if reader.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_SIZE {
            warn!(len, "frame too large, dropping connection");
            break;
        }

        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).await.is_err() {
            break;
        }

        if ipc::is_binary_frame(&payload) {
            match ipc::decode_pty_frame(&payload) {
                Ok((prompt_id, data)) => {
                    let _ = pty_tx.send(PtyFrame { prompt_id, data });
                }
                Err(e) => {
                    warn!("failed to decode PTY frame: {e}");
                }
            }
        } else {
            match serde_json::from_slice::<DaemonEvent>(&payload) {
                Ok(event) => {
                    // Send to response channel (for request-response matching)
                    // and broadcast to all subscribers.
                    let _ = response_tx.send(event.clone());
                    let _ = event_tx.send(event);
                }
                Err(e) => {
                    warn!("failed to deserialize daemon event: {e}");
                }
            }
        }
    }
}

/// Write requests to the daemon and match responses.
///
/// For each `PendingRequest`, serializes and sends the `ClientRequest`,
/// then waits for the next JSON response from the read loop to deliver back.
async fn write_loop(
    mut writer: tokio::io::WriteHalf<tokio::net::UnixStream>,
    request_rx: &mut mpsc::Receiver<PendingRequest>,
    response_rx: &mut mpsc::UnboundedReceiver<DaemonEvent>,
    _bridge: &Arc<DaemonBridge>,
) -> Result<(), BridgeError> {
    // Send an initial Subscribe so we receive broadcast events.
    let sub_json =
        serde_json::to_vec(&ClientRequest::Subscribe).map_err(|e| BridgeError::Serialize(e.to_string()))?;
    let sub_frame = ipc::encode_frame(&sub_json);
    writer
        .write_all(&sub_frame)
        .await
        .map_err(|_| BridgeError::Disconnected)?;
    writer
        .flush()
        .await
        .map_err(|_| BridgeError::Disconnected)?;
    debug!("sent Subscribe to daemon");

    loop {
        tokio::select! {
            // Handle outgoing requests.
            pending = request_rx.recv() => {
                let Some(pending) = pending else {
                    // All request senders dropped (bridge dropped).
                    return Ok(());
                };

                let json = match serde_json::to_vec(&pending.request) {
                    Ok(j) => j,
                    Err(e) => {
                        let _ = pending.response_tx.send(Err(BridgeError::Serialize(e.to_string())));
                        continue;
                    }
                };

                let frame = ipc::encode_frame(&json);
                if writer.write_all(&frame).await.is_err() {
                    let _ = pending.response_tx.send(Err(BridgeError::Disconnected));
                    return Err(BridgeError::Disconnected);
                }
                if writer.flush().await.is_err() {
                    let _ = pending.response_tx.send(Err(BridgeError::Disconnected));
                    return Err(BridgeError::Disconnected);
                }

                // Wait for the next response event from the read loop.
                // This is a simple FIFO matching — the daemon responds in order.
                match response_rx.recv().await {
                    Some(event) => {
                        let _ = pending.response_tx.send(Ok(event));
                    }
                    None => {
                        let _ = pending.response_tx.send(Err(BridgeError::Disconnected));
                        return Err(BridgeError::Disconnected);
                    }
                }
            }

            // Drain unsolicited events from the response channel so it doesn't
            // fill up (broadcast events that arrive between requests).
            event = response_rx.recv() => {
                if event.is_none() {
                    // Read loop exited.
                    return Err(BridgeError::Disconnected);
                }
                // Unsolicited event — already broadcast via event_tx in read_loop.
                // Just discard from the response channel.
            }
        }
    }
}

/// Drain and fail all pending requests in the channel.
async fn drain_pending_requests(request_rx: &mut mpsc::Receiver<PendingRequest>) {
    while let Ok(pending) = request_rx.try_recv() {
        let _ = pending
            .response_tx
            .send(Err(BridgeError::Disconnected));
    }
}
