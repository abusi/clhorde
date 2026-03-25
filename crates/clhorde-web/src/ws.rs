//! WebSocket handler for real-time daemon event streaming and PTY byte forwarding.
//!
//! Upgrades an HTTP connection to WebSocket, subscribes to daemon events and PTY
//! bytes, and fans out updates to the connected client. Also accepts `ClientRequest`
//! messages and PTY subscription control from the client.

use std::collections::HashSet;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use base64::prelude::*;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::bridge::PtyFrame;
use crate::state::AppState;

/// Envelope for server → client daemon event messages.
fn server_message(event: &DaemonEvent) -> Option<Message> {
    let envelope = json!({
        "type": "DaemonEvent",
        "event": event,
    });
    serde_json::to_string(&envelope)
        .ok()
        .map(Message::text)
}

/// Build a PtyBytes message from a PTY frame.
fn pty_message(frame: &PtyFrame) -> Option<Message> {
    let envelope = json!({
        "type": "PtyBytes",
        "prompt_id": frame.prompt_id,
        "data": BASE64_STANDARD.encode(&frame.data),
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
    /// Present for ClientRequest messages.
    #[serde(default)]
    request: serde_json::Value,
    /// Present for Subscribe/Unsubscribe messages.
    #[serde(default)]
    prompt_id: Option<usize>,
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
    let mut pty_rx = state.bridge.subscribe_pty();
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Per-client PTY subscription set. Empty = no PTY bytes forwarded.
    let mut pty_subscriptions: HashSet<usize> = HashSet::new();

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
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "WebSocket client lagged on events");
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

            // Forward PTY bytes for subscribed prompts.
            frame = pty_rx.recv() => {
                match frame {
                    Ok(pty_frame) => {
                        if pty_subscriptions.contains(&pty_frame.prompt_id) {
                            if let Some(msg) = pty_message(&pty_frame) {
                                if ws_tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "WebSocket client lagged on PTY bytes");
                        // PTY lag is less critical — just skip silently.
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // PTY channel closed — daemon disconnected (handled by event channel).
                    }
                }
            }

            // Handle incoming messages from the WebSocket client.
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_client_message(
                            &text,
                            &state,
                            &mut ws_tx,
                            &mut pty_subscriptions,
                        ).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break;
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
    pty_subscriptions: &mut HashSet<usize>,
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

    match envelope.msg_type.as_str() {
        "ClientRequest" => {
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

        "SubscribePty" => {
            if let Some(prompt_id) = envelope.prompt_id {
                pty_subscriptions.insert(prompt_id);
                debug!(prompt_id, "client subscribed to PTY bytes");
                let ack = json!({
                    "type": "PtySubscribed",
                    "prompt_id": prompt_id,
                });
                if let Ok(text) = serde_json::to_string(&ack) {
                    let _ = ws_tx.send(Message::text(text)).await;
                }
            } else {
                let err = json!({
                    "type": "Error",
                    "error": "SubscribePty requires \"prompt_id\"",
                });
                if let Ok(text) = serde_json::to_string(&err) {
                    let _ = ws_tx.send(Message::text(text)).await;
                }
            }
        }

        "UnsubscribePty" => {
            if let Some(prompt_id) = envelope.prompt_id {
                pty_subscriptions.remove(&prompt_id);
                debug!(prompt_id, "client unsubscribed from PTY bytes");
                let ack = json!({
                    "type": "PtyUnsubscribed",
                    "prompt_id": prompt_id,
                });
                if let Ok(text) = serde_json::to_string(&ack) {
                    let _ = ws_tx.send(Message::text(text)).await;
                }
            } else {
                let err = json!({
                    "type": "Error",
                    "error": "UnsubscribePty requires \"prompt_id\"",
                });
                if let Ok(text) = serde_json::to_string(&err) {
                    let _ = ws_tx.send(Message::text(text)).await;
                }
            }
        }

        other => {
            let err = json!({
                "type": "Error",
                "error": format!("unknown message type: {other}"),
            });
            if let Ok(text) = serde_json::to_string(&err) {
                let _ = ws_tx.send(Message::text(text)).await;
            }
        }
    }
}
