//! Optional token-based authentication middleware.
//!
//! When an auth token is configured, all `/api/*` requests must include a valid
//! `Authorization: Bearer <token>` header. WebSocket upgrade requests may also
//! pass the token as a `?token=<token>` query parameter.
//!
//! Static file requests (non-`/api/` paths) are not authenticated — the login
//! page must be accessible without a token.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Auth middleware that checks for a Bearer token on `/api/*` routes.
pub async fn auth_middleware(
    req: Request<Body>,
    next: Next,
) -> Response {
    let token = match req.extensions().get::<AuthToken>() {
        Some(t) => &t.0,
        None => return next.run(req).await, // No auth configured
    };

    let path = req.uri().path();

    // Only protect API routes
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }

    // Check Authorization header first
    if let Some(auth_header) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(value) = auth_header.to_str() {
            if let Some(bearer_token) = value.strip_prefix("Bearer ") {
                if bearer_token == token {
                    return next.run(req).await;
                }
            }
        }
    }

    // Check query parameter (for WebSocket upgrades)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("token=") {
                if value == token {
                    return next.run(req).await;
                }
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "error": "Authentication required" })),
    )
        .into_response()
}

/// Extension type that carries the auth token into the middleware.
#[derive(Clone)]
pub struct AuthToken(pub String);
