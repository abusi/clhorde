//! REST API route handlers.
//!
//! M3: Read-only endpoints (health, state, prompts)
//! M4: Mutation endpoints (submit, input, kill, retry, resume, delete, move)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::bridge::BridgeError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    Router::new()
        // M3: read-only
        .route("/api/health", get(health))
        .route("/api/state", get(get_state))
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts/{id}", get(get_prompt))
        .route("/api/prompts/{id}/output", get(get_prompt_output))
        // M4: mutations
        .route("/api/prompts", post(submit_prompt))
        .route("/api/prompts/{id}/input", post(send_input))
        .route("/api/prompts/{id}/kill", post(kill_worker))
        .route("/api/prompts/{id}/retry", post(retry_prompt))
        .route("/api/prompts/{id}/resume", post(resume_prompt))
        .route("/api/prompts/{id}", delete(delete_prompt))
        .route("/api/prompts/{id}/move-up", post(move_prompt_up))
        .route("/api/prompts/{id}/move-down", post(move_prompt_down))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn error_json(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
}

impl From<&BridgeError> for StatusCode {
    fn from(e: &BridgeError) -> Self {
        match e {
            BridgeError::Connect(_) => StatusCode::BAD_GATEWAY,
            BridgeError::Io(_) => StatusCode::BAD_GATEWAY,
            BridgeError::Protocol(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

fn bridge_error(e: BridgeError) -> impl IntoResponse {
    let status = StatusCode::from(&e);
    error_json(status, e.to_string())
}

/// Parse a path segment as a prompt ID (usize).
fn parse_prompt_id(id: &str) -> Result<usize, (StatusCode, Json<ErrorResponse>)> {
    id.parse::<usize>()
        .map_err(|_| error_json(StatusCode::BAD_REQUEST, format!("invalid prompt id: {id}")))
}

// ---------------------------------------------------------------------------
// M3: Read-only endpoints
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.request(ClientRequest::Ping).await {
        Ok(DaemonEvent::Pong) => {
            Json(serde_json::json!({ "status": "ok" })).into_response()
        }
        Ok(_) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected daemon response",
        )
        .into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

async fn get_state(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.request(ClientRequest::GetState).await {
        Ok(DaemonEvent::StateSnapshot(daemon_state)) => {
            Json(serde_json::to_value(daemon_state).unwrap()).into_response()
        }
        Ok(DaemonEvent::Error { message }) => {
            error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
        }
        Ok(_) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected daemon response",
        )
        .into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

async fn list_prompts(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.request(ClientRequest::GetState).await {
        Ok(DaemonEvent::StateSnapshot(daemon_state)) => {
            Json(serde_json::to_value(daemon_state.prompts).unwrap()).into_response()
        }
        Ok(DaemonEvent::Error { message }) => {
            error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
        }
        Ok(_) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected daemon response",
        )
        .into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

async fn get_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let prompt_id = match parse_prompt_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };

    match state.bridge.request(ClientRequest::GetState).await {
        Ok(DaemonEvent::StateSnapshot(daemon_state)) => {
            match daemon_state.prompts.into_iter().find(|p| p.id == prompt_id) {
                Some(prompt) => Json(serde_json::to_value(prompt).unwrap()).into_response(),
                None => error_json(
                    StatusCode::NOT_FOUND,
                    format!("prompt {prompt_id} not found"),
                )
                .into_response(),
            }
        }
        Ok(DaemonEvent::Error { message }) => {
            error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
        }
        Ok(_) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected daemon response",
        )
        .into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

async fn get_prompt_output(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let prompt_id = match parse_prompt_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };

    match state
        .bridge
        .request(ClientRequest::GetPromptOutput { prompt_id })
        .await
    {
        Ok(DaemonEvent::PromptOutput { full_text, .. }) => {
            Json(serde_json::json!({ "prompt_id": prompt_id, "output": full_text }))
                .into_response()
        }
        Ok(DaemonEvent::Error { message }) => {
            // Daemon returns Error for unknown prompt IDs
            if message.contains("not found") || message.contains("No prompt") {
                error_json(StatusCode::NOT_FOUND, message).into_response()
            } else {
                error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
            }
        }
        Ok(_) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected daemon response",
        )
        .into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// M4: Mutation endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
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

async fn submit_prompt(
    State(state): State<AppState>,
    body: Result<Json<SubmitPromptBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error_json(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    if body.text.trim().is_empty() {
        return error_json(StatusCode::BAD_REQUEST, "text must not be empty").into_response();
    }

    match body.mode.as_str() {
        "interactive" | "one-shot" => {}
        other => {
            return error_json(
                StatusCode::BAD_REQUEST,
                format!("invalid mode: {other} (expected \"interactive\" or \"one-shot\")"),
            )
            .into_response();
        }
    }

    let req = ClientRequest::SubmitPrompt {
        text: body.text,
        cwd: body.cwd,
        mode: body.mode,
        worktree: body.worktree,
        tags: body.tags,
    };

    match state.bridge.request(req).await {
        Ok(DaemonEvent::PromptAdded(info)) => {
            (StatusCode::CREATED, Json(serde_json::to_value(info).unwrap())).into_response()
        }
        Ok(DaemonEvent::Error { message }) => {
            error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
        }
        Ok(_) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected daemon response",
        )
        .into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

#[derive(Deserialize)]
struct SendInputBody {
    text: String,
}

async fn send_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<SendInputBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let prompt_id = match parse_prompt_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };

    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error_json(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let req = ClientRequest::SendInput {
        prompt_id,
        text: body.text,
    };

    match state.bridge.request(req).await {
        Ok(DaemonEvent::Error { message }) => {
            if message.contains("not found") || message.contains("No prompt") {
                error_json(StatusCode::NOT_FOUND, message).into_response()
            } else {
                error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
            }
        }
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

/// Helper for simple prompt-id-only action endpoints (kill, retry, resume, move).
async fn prompt_action(
    state: &AppState,
    id: &str,
    make_request: impl FnOnce(usize) -> ClientRequest,
) -> axum::response::Response {
    let prompt_id = match parse_prompt_id(id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };

    let req = make_request(prompt_id);

    match state.bridge.request(req).await {
        Ok(DaemonEvent::PromptAdded(info) | DaemonEvent::PromptUpdated(info)) => {
            Json(serde_json::to_value(info).unwrap()).into_response()
        }
        Ok(DaemonEvent::Error { message }) => {
            if message.contains("not found") || message.contains("No prompt") {
                error_json(StatusCode::NOT_FOUND, message).into_response()
            } else {
                error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
            }
        }
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

async fn kill_worker(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    prompt_action(&state, &id, |prompt_id| ClientRequest::KillWorker {
        prompt_id,
    })
    .await
}

async fn retry_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    prompt_action(&state, &id, |prompt_id| ClientRequest::RetryPrompt {
        prompt_id,
    })
    .await
}

async fn resume_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    prompt_action(&state, &id, |prompt_id| ClientRequest::ResumePrompt {
        prompt_id,
    })
    .await
}

async fn delete_prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let prompt_id = match parse_prompt_id(&id) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };

    match state
        .bridge
        .request(ClientRequest::DeletePrompt { prompt_id })
        .await
    {
        Ok(DaemonEvent::PromptRemoved { .. }) => StatusCode::NO_CONTENT.into_response(),
        Ok(DaemonEvent::Error { message }) => {
            if message.contains("not found") || message.contains("No prompt") {
                error_json(StatusCode::NOT_FOUND, message).into_response()
            } else {
                error_json(StatusCode::INTERNAL_SERVER_ERROR, message).into_response()
            }
        }
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bridge_error(e).into_response(),
    }
}

async fn move_prompt_up(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    prompt_action(&state, &id, |prompt_id| ClientRequest::MovePromptUp {
        prompt_id,
    })
    .await
}

async fn move_prompt_down(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    prompt_action(&state, &id, |prompt_id| ClientRequest::MovePromptDown {
        prompt_id,
    })
    .await
}
