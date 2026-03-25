//! WebSocket handler for real-time daemon event streaming.
//!
//! Upgrades an HTTP connection to WebSocket, subscribes to daemon events,
//! and fans out updates to the connected client. Also accepts `ClientRequest`
//! messages from the client and forwards them to the daemon bridge.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::state::AppState;

/// Envelope for server → client messages.
fn server_message(event: &DaemonEvent) -> Option<Message> {
    let envelope = json!({
        "type": "DaemonEvent",
        "event": event,
    });
    serde_json::to_string(&envelope)
        .ok()
        .map(Message::text)
}

/// Envelope the client sends to the server.
#[derive(Debug, Deserialize)]
struct ClientEnvelope {
    #[serde(rename = "type")]
    msg_type: String,
    request: serde_json::Value,
}

/// `GET /api/ws` — upgrade to WebSocket.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Main WebSocket connection handler.
async fn handle_ws(socket: WebSocket, state: AppState) {
    let count = state.ws_connect();
    info!(ws_connections = count, "WebSocket client connected");

    let mut event_rx = state.bridge.subscribe_events();
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Send the current state snapshot as the first message so the client
    // doesn't need a separate REST call to bootstrap.
    if let Ok(snapshot_event) = state.bridge.send_request(ClientRequest::GetState).await {
        if let Some(msg) = server_message(&snapshot_event) {
            if ws_tx.send(msg).await.is_err() {
                let count = state.ws_disconnect();
                debug!(ws_connections = count, "WebSocket client disconnected during init");
                return;
            }
        }
    }

    loop {
        tokio::select! {
            // Forward daemon events to the WebSocket client.
            event = event_rx.recv() => {
                match event {
                    Ok(daemon_event) => {
                        if let Some(msg) = server_message(&daemon_event) {
                            if ws_tx.send(msg).await.is_err() {
                                break; // Client disconnected
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "WebSocket client lagged, skipping events");
                        // Send a notification so the client knows it missed events.
                        let lag_msg = json!({
                            "type": "Error",
                            "error": format!("lagged: missed {n} events"),
                        });
                        if let Ok(text) = serde_json::to_string(&lag_msg) {
                            if ws_tx.send(Message::text(text)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Daemon bridge dropped the broadcast sender — daemon disconnected.
                        let err_msg = json!({
                            "type": "Error",
                            "error": "daemon disconnected",
                        });
                        if let Ok(text) = serde_json::to_string(&err_msg) {
                            let _ = ws_tx.send(Message::text(text)).await;
                        }
                        break;
                    }
                }
            }

            // Handle incoming messages from the WebSocket client.
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(&text, &state, &mut ws_tx).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break; // Client disconnected
                    }
                    Some(Ok(_)) => {
                        // Ignore binary, ping, pong — axum handles ping/pong automatically.
                    }
                    Some(Err(e)) => {
                        debug!(error = %e, "WebSocket receive error");
                        break;
                    }
                }
            }
        }
    }

    let count = state.ws_disconnect();
    info!(ws_connections = count, "WebSocket client disconnected");
}

/// Parse and handle a client message, sending the response back over WebSocket.
async fn handle_client_message(
    text: &str,
    state: &AppState,
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) {
    let envelope: ClientEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            let err = json!({
                "type": "Error",
                "error": format!("invalid message: {e}"),
            });
            if let Ok(text) = serde_json::to_string(&err) {
                let _ = ws_tx.send(Message::text(text)).await;
            }
            return;
        }
    };

    if envelope.msg_type != "ClientRequest" {
        let err = json!({
            "type": "Error",
            "error": format!("unknown message type: {}", envelope.msg_type),
        });
        if let Ok(text) = serde_json::to_string(&err) {
            let _ = ws_tx.send(Message::text(text)).await;
        }
        return;
    }

    let request: ClientRequest = match serde_json::from_value(envelope.request) {
        Ok(r) => r,
        Err(e) => {
            let err = json!({
                "type": "Error",
                "error": format!("invalid request: {e}"),
            });
            if let Ok(text) = serde_json::to_string(&err) {
                let _ = ws_tx.send(Message::text(text)).await;
            }
            return;
        }
    };

    // Forward the request to the daemon and send the response back.
    let response = match state.bridge.send_request(request).await {
        Ok(event) => json!({
            "type": "DaemonEvent",
            "event": event,
        }),
        Err(e) => json!({
            "type": "Error",
            "error": format!("bridge error: {e}"),
        }),
    };

    if let Ok(text) = serde_json::to_string(&response) {
        let _ = ws_tx.send(Message::text(text)).await;
    }
}
