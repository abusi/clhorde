//! REST API route handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::state::AppState;

/// Build the axum router with all REST API routes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(get_state))
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts/{id}", get(get_prompt))
        .route("/api/prompts/{id}/output", get(get_prompt_output))
        .with_state(state)
}

/// `GET /api/health` — ping the daemon, return `{ "status": "ok" }`.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.send_request(ClientRequest::Ping).await {
        Ok(DaemonEvent::Pong) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        ),
        Err(e) => {
            tracing::error!("health check failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "daemon unavailable" })),
            )
        }
    }
}

/// `GET /api/state` — full daemon state snapshot.
async fn get_state(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.send_request(ClientRequest::GetState).await {
        Ok(DaemonEvent::StateSnapshot(daemon_state)) => {
            (StatusCode::OK, Json(json!(daemon_state))).into_response()
        }
        Ok(DaemonEvent::Error { message }) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
            .into_response(),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("get_state failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "daemon unavailable" })),
            )
                .into_response()
        }
    }
}

/// `GET /api/prompts` — list all prompts (extracted from state).
async fn list_prompts(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.send_request(ClientRequest::GetState).await {
        Ok(DaemonEvent::StateSnapshot(daemon_state)) => {
            (StatusCode::OK, Json(json!(daemon_state.prompts))).into_response()
        }
        Ok(DaemonEvent::Error { message }) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
            .into_response(),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("list_prompts failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "daemon unavailable" })),
            )
                .into_response()
        }
    }
}

/// `GET /api/prompts/:id` — single prompt info, 404 if not found.
async fn get_prompt(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    match state.bridge.send_request(ClientRequest::GetState).await {
        Ok(DaemonEvent::StateSnapshot(daemon_state)) => {
            match daemon_state.prompts.into_iter().find(|p| p.id == id) {
                Some(prompt) => (StatusCode::OK, Json(json!(prompt))).into_response(),
                None => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": format!("prompt {id} not found") })),
                )
                    .into_response(),
            }
        }
        Ok(DaemonEvent::Error { message }) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": message })),
        )
            .into_response(),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("get_prompt failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "daemon unavailable" })),
            )
                .into_response()
        }
    }
}

/// `GET /api/prompts/:id/output` — full output text for a prompt.
async fn get_prompt_output(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    match state
        .bridge
        .send_request(ClientRequest::GetPromptOutput { prompt_id: id })
        .await
    {
        Ok(DaemonEvent::PromptOutput { prompt_id, full_text }) => (
            StatusCode::OK,
            Json(json!({ "prompt_id": prompt_id, "output": full_text })),
        )
            .into_response(),
        Ok(DaemonEvent::Error { message }) => {
            // The daemon returns an error for unknown prompt IDs
            let status = if message.contains("not found") || message.contains("No prompt") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": message }))).into_response()
        }
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("get_prompt_output failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "daemon unavailable" })),
            )
                .into_response()
        }
    }
}
