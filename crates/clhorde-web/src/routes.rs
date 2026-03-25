//! REST API route handlers.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::state::AppState;

/// Build the axum router with all REST API routes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(get_state))
        .route("/api/prompts", get(list_prompts).post(submit_prompt))
        .route(
            "/api/prompts/{id}",
            get(get_prompt).delete(delete_prompt),
        )
        .route("/api/prompts/{id}/output", get(get_prompt_output))
        .route("/api/prompts/{id}/input", post(send_input))
        .route("/api/prompts/{id}/kill", post(kill_worker))
        .route("/api/prompts/{id}/retry", post(retry_prompt))
        .route("/api/prompts/{id}/resume", post(resume_prompt))
        .route("/api/prompts/{id}/move-up", post(move_prompt_up))
        .route("/api/prompts/{id}/move-down", post(move_prompt_down))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// Request body for `POST /api/prompts`.
#[derive(Debug, Deserialize)]
struct SubmitPromptBody {
    text: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    worktree: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

fn default_mode() -> String {
    "interactive".to_string()
}

/// Request body for `POST /api/prompts/:id/input`.
#[derive(Debug, Deserialize)]
struct SendInputBody {
    text: String,
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

// ---------------------------------------------------------------------------
// Mutation endpoints
// ---------------------------------------------------------------------------

/// `POST /api/prompts` — submit a new prompt.
async fn submit_prompt(
    State(state): State<AppState>,
    Json(body): Json<SubmitPromptBody>,
) -> impl IntoResponse {
    // Validate required field
    if body.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "\"text\" must not be empty" })),
        )
            .into_response();
    }

    // Validate mode
    if !is_valid_mode(&body.mode) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "invalid mode \"{}\": expected \"interactive\" or \"one-shot\"",
                    body.mode
                )
            })),
        )
            .into_response();
    }

    let request = ClientRequest::SubmitPrompt {
        text: body.text,
        cwd: body.cwd,
        mode: body.mode,
        worktree: body.worktree,
        tags: body.tags,
    };

    match state.bridge.send_request(request).await {
        Ok(DaemonEvent::PromptAdded(info)) => {
            (StatusCode::CREATED, Json(json!(info))).into_response()
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
            tracing::error!("submit_prompt failed: {e}");
            daemon_unavailable()
        }
    }
}

/// `POST /api/prompts/:id/input` — send follow-up input to a running worker.
async fn send_input(
    State(state): State<AppState>,
    Path(id): Path<usize>,
    Json(body): Json<SendInputBody>,
) -> impl IntoResponse {
    if body.text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "\"text\" must not be empty" })),
        )
            .into_response();
    }

    let request = ClientRequest::SendInput {
        prompt_id: id,
        text: body.text,
    };

    dispatch_action(&state, request, "send_input").await
}

/// `POST /api/prompts/:id/kill` — kill a running worker.
async fn kill_worker(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    dispatch_action(&state, ClientRequest::KillWorker { prompt_id: id }, "kill_worker").await
}

/// `POST /api/prompts/:id/retry` — retry a completed/failed prompt.
async fn retry_prompt(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    dispatch_prompt_action(&state, ClientRequest::RetryPrompt { prompt_id: id }, "retry_prompt")
        .await
}

/// `POST /api/prompts/:id/resume` — resume a completed/failed prompt with `--resume`.
async fn resume_prompt(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    dispatch_prompt_action(
        &state,
        ClientRequest::ResumePrompt { prompt_id: id },
        "resume_prompt",
    )
    .await
}

/// `DELETE /api/prompts/:id` — delete a prompt.
async fn delete_prompt(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    let request = ClientRequest::DeletePrompt { prompt_id: id };

    match state.bridge.send_request(request).await {
        Ok(DaemonEvent::PromptRemoved { .. }) => StatusCode::NO_CONTENT.into_response(),
        Ok(DaemonEvent::Error { message }) => daemon_error_to_response(&message),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("delete_prompt failed: {e}");
            daemon_unavailable()
        }
    }
}

/// `POST /api/prompts/:id/move-up` — move a pending prompt up in the queue.
async fn move_prompt_up(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    dispatch_action(
        &state,
        ClientRequest::MovePromptUp { prompt_id: id },
        "move_prompt_up",
    )
    .await
}

/// `POST /api/prompts/:id/move-down` — move a pending prompt down in the queue.
async fn move_prompt_down(
    State(state): State<AppState>,
    Path(id): Path<usize>,
) -> impl IntoResponse {
    dispatch_action(
        &state,
        ClientRequest::MovePromptDown { prompt_id: id },
        "move_prompt_down",
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate a prompt mode string.
fn is_valid_mode(mode: &str) -> bool {
    matches!(
        mode,
        "interactive" | "one-shot" | "one_shot" | "oneshot"
    )
}

/// Standard "daemon unavailable" error response.
fn daemon_unavailable() -> axum::response::Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": "daemon unavailable" })),
    )
        .into_response()
}

/// Map a daemon error message to a 404 (if "not found") or 500 response.
fn daemon_error_to_response(message: &str) -> axum::response::Response {
    let status = if message.contains("not found") || message.contains("No prompt") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(json!({ "error": message }))).into_response()
}

/// Dispatch a simple action request that expects `PromptUpdated`, `Error`, or
/// a generic acknowledgement. Returns `200` with prompt info on success.
async fn dispatch_prompt_action(
    state: &AppState,
    request: ClientRequest,
    label: &str,
) -> axum::response::Response {
    match state.bridge.send_request(request).await {
        Ok(DaemonEvent::PromptAdded(info) | DaemonEvent::PromptUpdated(info)) => {
            (StatusCode::OK, Json(json!(info))).into_response()
        }
        Ok(DaemonEvent::Error { message }) => daemon_error_to_response(&message),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "unexpected response from daemon" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("{label} failed: {e}");
            daemon_unavailable()
        }
    }
}

/// Dispatch a simple action request that only needs success/error (no prompt
/// info in the response). Returns `200 { "ok": true }` on any non-error event.
async fn dispatch_action(
    state: &AppState,
    request: ClientRequest,
    label: &str,
) -> axum::response::Response {
    match state.bridge.send_request(request).await {
        Ok(DaemonEvent::Error { message }) => daemon_error_to_response(&message),
        Ok(_) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => {
            tracing::error!("{label} failed: {e}");
            daemon_unavailable()
        }
    }
}
