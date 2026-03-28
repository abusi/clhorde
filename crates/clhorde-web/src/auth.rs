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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Build a minimal app with auth middleware for testing.
    ///
    /// Layer order matters: middleware must run after Extension has been inserted.
    /// `.layer()` applies outer-first, so we add middleware first (outer), then
    /// Extension (inner). This means Extension runs first, inserting the token,
    /// then middleware sees it.
    fn app_with_token(token: &str) -> Router {
        Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/index.html", get(|| async { "page" }))
            .layer(middleware::from_fn(auth_middleware))
            .layer(axum::Extension(AuthToken(token.to_string())))
    }

    /// Build an app without auth (no token configured).
    fn app_without_token() -> Router {
        Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/index.html", get(|| async { "page" }))
            .layer(middleware::from_fn(auth_middleware))
    }

    #[tokio::test]
    async fn no_token_configured_allows_all() {
        let resp = app_without_token()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_token_returns_401() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Authentication required");
    }

    #[tokio::test]
    async fn valid_bearer_token_passes() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "Bearer secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_bearer_token_returns_401() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn query_param_token_passes() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health?token=secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn query_param_wrong_token_returns_401() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health?token=wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn static_files_bypass_auth() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Non-/api/ routes should pass through without auth
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_prefix_required() {
        // Authorization header without "Bearer " prefix should fail
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_among_multiple_query_params() {
        let resp = app_with_token("secret123")
            .oneshot(
                Request::builder()
                    .uri("/api/health?foo=bar&token=secret123&baz=qux")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
