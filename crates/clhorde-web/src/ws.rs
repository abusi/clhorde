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

use clhorde_core::control::{ControlRequest, ControlResponse, SchedulerEvent};
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

/// Envelope for scheduler push events. Mirrors the daemon's
/// `DaemonEvent` envelope so the SPA can route on `type` alone.
fn scheduler_message(event: &SchedulerEvent) -> Option<Message> {
    let envelope = json!({
        "type": "SchedulerEvent",
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
    let mut sched_rx = state.scheduler.subscribe_events();
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

    // Bootstrap scheduler state: if the bridge is alive, request a
    // one-shot Status so the client has something to show before the
    // next push event arrives. Wrapped in a Snapshot envelope so the
    // SPA dispatch code is identical for bootstrap and push paths.
    if state.scheduler.is_connected() {
        if let Ok(ControlResponse::Status { workflows, root }) = state
            .scheduler
            .request(ControlRequest::Status { name: None })
            .await
        {
            let snapshot = SchedulerEvent::Snapshot { workflows, root };
            if let Some(msg) = scheduler_message(&snapshot) {
                if ws_tx.send(msg).await.is_err() {
                    let count = state.ws_disconnect();
                    debug!(ws_connections = count, "WebSocket client disconnected during scheduler init");
                    return;
                }
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

            // Forward scheduler events to the WebSocket client.
            // Lag is non-fatal: the scheduler-side server re-emits a
            // Snapshot when it detects a slow subscriber, so we just
            // skip and let the next event arrive.
            sched_event = sched_rx.recv() => {
                match sched_event {
                    Ok(event) => {
                        if let Some(msg) = scheduler_message(&event) {
                            if ws_tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!(missed = n, "WebSocket client lagged on scheduler events");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Bridge dropped — clhorde-web shutting down.
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

#[cfg(test)]
mod tests {
    use super::*;
    use clhorde_core::protocol::{DaemonEvent, DaemonState};

    // -----------------------------------------------------------------------
    // server_message — DaemonEvent → JSON envelope
    // -----------------------------------------------------------------------

    #[test]
    fn server_message_wraps_pong() {
        let msg = server_message(&DaemonEvent::Pong).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "DaemonEvent");
        assert_eq!(json["event"]["type"], "Pong");
    }

    #[test]
    fn server_message_wraps_worker_started() {
        let msg = server_message(&DaemonEvent::WorkerStarted { prompt_id: 42 }).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "DaemonEvent");
        assert_eq!(json["event"]["type"], "WorkerStarted");
        assert_eq!(json["event"]["prompt_id"], 42);
    }

    #[test]
    fn server_message_wraps_worker_finished() {
        let msg = server_message(&DaemonEvent::WorkerFinished {
            prompt_id: 7,
            exit_code: Some(0),
        })
        .unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "WorkerFinished");
        assert_eq!(json["event"]["prompt_id"], 7);
        assert_eq!(json["event"]["exit_code"], 0);
    }

    #[test]
    fn server_message_wraps_worker_error() {
        let msg = server_message(&DaemonEvent::WorkerError {
            prompt_id: 3,
            error: "timeout".to_string(),
        })
        .unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "WorkerError");
        assert_eq!(json["event"]["error"], "timeout");
    }

    #[test]
    fn server_message_wraps_output_chunk() {
        let msg = server_message(&DaemonEvent::OutputChunk {
            prompt_id: 1,
            text: "hello world".to_string(),
        })
        .unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "OutputChunk");
        assert_eq!(json["event"]["prompt_id"], 1);
        assert_eq!(json["event"]["text"], "hello world");
    }

    #[test]
    fn server_message_wraps_prompt_removed() {
        let msg = server_message(&DaemonEvent::PromptRemoved { prompt_id: 5 }).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "PromptRemoved");
        assert_eq!(json["event"]["prompt_id"], 5);
    }

    #[test]
    fn server_message_wraps_max_workers_changed() {
        let msg = server_message(&DaemonEvent::MaxWorkersChanged { count: 10 }).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "MaxWorkersChanged");
        assert_eq!(json["event"]["count"], 10);
    }

    #[test]
    fn server_message_wraps_error() {
        let msg = server_message(&DaemonEvent::Error {
            message: "something went wrong".to_string(),
        })
        .unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "Error");
        assert_eq!(json["event"]["message"], "something went wrong");
    }

    #[test]
    fn server_message_wraps_state_snapshot() {
        let state = DaemonState {
            prompts: vec![],
            max_workers: 4,
            active_workers: 1,
            default_mode: "interactive".to_string(),
            protocol_version: 1,
        };
        let msg = server_message(&DaemonEvent::StateSnapshot(state)).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "DaemonEvent");
        assert_eq!(json["event"]["type"], "StateSnapshot");
        assert_eq!(json["event"]["max_workers"], 4);
        assert_eq!(json["event"]["active_workers"], 1);
    }

    #[test]
    fn server_message_wraps_store_count_result() {
        let msg = server_message(&DaemonEvent::StoreCountResult {
            pending: 2,
            running: 1,
            completed: 5,
            failed: 0,
        })
        .unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["event"]["type"], "StoreCountResult");
        assert_eq!(json["event"]["pending"], 2);
        assert_eq!(json["event"]["completed"], 5);
    }

    // -----------------------------------------------------------------------
    // pty_message — PTY frame → base64 JSON envelope
    // -----------------------------------------------------------------------

    #[test]
    fn pty_message_base64_encodes_data() {
        let frame = PtyFrame {
            prompt_id: 3,
            data: vec![0x1b, 0x5b, 0x33, 0x31, 0x6d], // ESC[31m
        };
        let msg = pty_message(&frame).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "PtyBytes");
        assert_eq!(json["prompt_id"], 3);

        // Verify base64 round-trip
        let encoded = json["data"].as_str().unwrap();
        let decoded = BASE64_STANDARD.decode(encoded).unwrap();
        assert_eq!(decoded, vec![0x1b, 0x5b, 0x33, 0x31, 0x6d]);
    }

    #[test]
    fn pty_message_empty_data() {
        let frame = PtyFrame {
            prompt_id: 0,
            data: vec![],
        };
        let msg = pty_message(&frame).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "PtyBytes");
        let encoded = json["data"].as_str().unwrap();
        let decoded = BASE64_STANDARD.decode(encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn pty_message_large_frame() {
        let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let frame = PtyFrame {
            prompt_id: 99,
            data: data.clone(),
        };
        let msg = pty_message(&frame).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        let encoded = json["data"].as_str().unwrap();
        let decoded = BASE64_STANDARD.decode(encoded).unwrap();
        assert_eq!(decoded, data);
    }

    // -----------------------------------------------------------------------
    // scheduler_message — SchedulerEvent → JSON envelope (Phase 5.2)
    // -----------------------------------------------------------------------

    #[test]
    fn scheduler_message_wraps_snapshot() {
        let event = SchedulerEvent::Snapshot {
            workflows: vec![],
            root: Some("/tmp/repo".into()),
        };
        let msg = scheduler_message(&event).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "SchedulerEvent");
        assert_eq!(json["event"]["type"], "snapshot");
        assert_eq!(json["event"]["root"], "/tmp/repo");
    }

    #[test]
    fn scheduler_message_wraps_workflow_updated() {
        let summary = clhorde_core::control::WorkflowSummary {
            name: "x".into(),
            status: "implementing".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec![],
        };
        let event = SchedulerEvent::WorkflowUpdated { summary };
        let msg = scheduler_message(&event).unwrap();
        let text = msg.into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["type"], "SchedulerEvent");
        assert_eq!(json["event"]["type"], "workflow_updated");
        assert_eq!(json["event"]["summary"]["name"], "x");
        assert_eq!(json["event"]["summary"]["status"], "implementing");
    }

    // -----------------------------------------------------------------------
    // ClientEnvelope deserialization
    // -----------------------------------------------------------------------

    #[test]
    fn client_envelope_parse_request() {
        let raw = r#"{"type":"ClientRequest","request":{"type":"Ping"}}"#;
        let env: ClientEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.msg_type, "ClientRequest");
        assert_eq!(env.request["type"], "Ping");
    }

    #[test]
    fn client_envelope_parse_subscribe() {
        let raw = r#"{"type":"SubscribePty","prompt_id":5}"#;
        let env: ClientEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.msg_type, "SubscribePty");
        assert_eq!(env.prompt_id, Some(5));
    }

    #[test]
    fn client_envelope_parse_unsubscribe() {
        let raw = r#"{"type":"UnsubscribePty","prompt_id":10}"#;
        let env: ClientEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.msg_type, "UnsubscribePty");
        assert_eq!(env.prompt_id, Some(10));
    }

    #[test]
    fn client_envelope_unknown_type() {
        let raw = r#"{"type":"FooBar"}"#;
        let env: ClientEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.msg_type, "FooBar");
        assert_eq!(env.prompt_id, None);
    }

    #[test]
    fn client_envelope_defaults_missing_fields() {
        let raw = r#"{"type":"SubscribePty"}"#;
        let env: ClientEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.prompt_id, None);
        assert!(env.request.is_null());
    }
}
