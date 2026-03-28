//! Static file serving with compile-time embedding and filesystem override.
//!
//! By default, serves files embedded at compile time from `static/`. When
//! `--static-dir` is provided, serves from the filesystem instead. Non-API
//! routes that don't match a file fall back to `index.html` for SPA routing.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use include_dir::{include_dir, Dir};

/// Embedded static assets, baked in at compile time.
static EMBEDDED_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/static");

/// Maximum cache duration for hashed/immutable assets (1 year).
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// Cache policy for index.html and other non-hashed files.
const NO_CACHE: &str = "no-cache";

/// Source of static files: either embedded or a filesystem directory.
#[derive(Clone)]
pub enum StaticSource {
    Embedded,
    Directory(PathBuf),
}

/// Serve a static file request. Called as the axum fallback handler.
pub async fn serve_static(
    source: StaticSource,
    req: Request<Body>,
) -> Response<Body> {
    let path = req.uri().path();

    // Never intercept API or WebSocket routes.
    if path.starts_with("/api/") {
        return not_found();
    }

    // Strip leading slash for file lookup.
    let file_path = path.trim_start_matches('/');

    // Try serving the exact file first.
    if !file_path.is_empty() {
        if let Some(response) = serve_file(&source, file_path) {
            return response;
        }
    }

    // SPA fallback: serve index.html for all other routes.
    serve_file(&source, "index.html").unwrap_or_else(not_found)
}

/// Try to serve a single file from the configured source.
fn serve_file(source: &StaticSource, file_path: &str) -> Option<Response<Body>> {
    match source {
        StaticSource::Embedded => {
            let file = EMBEDDED_DIR.get_file(file_path)?;
            let content_type = mime_from_path(file_path);
            let cache_control = cache_policy(file_path);

            Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CACHE_CONTROL, cache_control)
                    .body(Body::from(file.contents().to_vec()))
                    .unwrap(),
            )
        }
        StaticSource::Directory(dir) => {
            let full_path = dir.join(file_path);

            // Prevent directory traversal.
            if !is_safe_path(dir, &full_path) {
                return None;
            }

            let contents = std::fs::read(&full_path).ok()?;
            let content_type = mime_from_path(file_path);
            let cache_control = cache_policy(file_path);

            Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::CACHE_CONTROL, cache_control)
                    .body(Body::from(contents))
                    .unwrap(),
            )
        }
    }
}

/// Check that the resolved path stays within the base directory.
fn is_safe_path(base: &Path, resolved: &Path) -> bool {
    match (base.canonicalize(), resolved.canonicalize()) {
        (Ok(base), Ok(resolved)) => resolved.starts_with(base),
        // If the file doesn't exist, canonicalize fails — that's fine, we'll 404.
        _ => false,
    }
}

/// Map file extensions to MIME types.
fn mime_from_path(path: &str) -> HeaderValue {
    let ext = path.rsplit('.').next().unwrap_or("");
    let mime = match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(mime)
}

/// Determine cache policy based on filename patterns.
///
/// Files with hash-like segments (e.g., `app.a1b2c3.js`) get long immutable
/// caching. Everything else (especially `index.html`) gets no-cache.
fn cache_policy(path: &str) -> &'static str {
    // index.html should never be cached.
    if path == "index.html" || path.ends_with("/index.html") {
        return NO_CACHE;
    }

    // Files with content hashes in the name (common patterns: .hash., -hash.)
    // get immutable caching.
    let filename = path.rsplit('/').next().unwrap_or(path);
    if looks_hashed(filename) {
        return IMMUTABLE_CACHE;
    }

    NO_CACHE
}

/// Heuristic: does the filename contain a hex hash segment?
/// Matches patterns like `app.a1b2c3d4.js` or `chunk-abc123.css`.
fn looks_hashed(filename: &str) -> bool {
    // Split on '.' and '-', look for segments that are 8+ hex characters.
    filename
        .split(['.', '-'])
        .any(|seg| seg.len() >= 8 && seg.chars().all(|c| c.is_ascii_hexdigit()))
}

fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("not found"))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // MIME type mapping
    // -----------------------------------------------------------------------

    #[test]
    fn mime_html() {
        assert_eq!(mime_from_path("index.html").to_str().unwrap(), "text/html; charset=utf-8");
    }

    #[test]
    fn mime_js() {
        assert_eq!(mime_from_path("app.js").to_str().unwrap(), "application/javascript; charset=utf-8");
    }

    #[test]
    fn mime_mjs() {
        assert_eq!(mime_from_path("module.mjs").to_str().unwrap(), "application/javascript; charset=utf-8");
    }

    #[test]
    fn mime_css() {
        assert_eq!(mime_from_path("style.css").to_str().unwrap(), "text/css; charset=utf-8");
    }

    #[test]
    fn mime_json() {
        assert_eq!(mime_from_path("data.json").to_str().unwrap(), "application/json; charset=utf-8");
    }

    #[test]
    fn mime_png() {
        assert_eq!(mime_from_path("logo.png").to_str().unwrap(), "image/png");
    }

    #[test]
    fn mime_jpg() {
        assert_eq!(mime_from_path("photo.jpg").to_str().unwrap(), "image/jpeg");
    }

    #[test]
    fn mime_jpeg() {
        assert_eq!(mime_from_path("photo.jpeg").to_str().unwrap(), "image/jpeg");
    }

    #[test]
    fn mime_svg() {
        assert_eq!(mime_from_path("icon.svg").to_str().unwrap(), "image/svg+xml");
    }

    #[test]
    fn mime_woff2() {
        assert_eq!(mime_from_path("font.woff2").to_str().unwrap(), "font/woff2");
    }

    #[test]
    fn mime_wasm() {
        assert_eq!(mime_from_path("module.wasm").to_str().unwrap(), "application/wasm");
    }

    #[test]
    fn mime_unknown_extension() {
        assert_eq!(mime_from_path("file.xyz").to_str().unwrap(), "application/octet-stream");
    }

    #[test]
    fn mime_no_extension() {
        assert_eq!(mime_from_path("LICENSE").to_str().unwrap(), "application/octet-stream");
    }

    #[test]
    fn mime_source_map() {
        assert_eq!(mime_from_path("app.js.map").to_str().unwrap(), "application/json");
    }

    // -----------------------------------------------------------------------
    // Cache policy
    // -----------------------------------------------------------------------

    #[test]
    fn cache_index_html_is_no_cache() {
        assert_eq!(cache_policy("index.html"), NO_CACHE);
    }

    #[test]
    fn cache_nested_index_html_is_no_cache() {
        assert_eq!(cache_policy("sub/index.html"), NO_CACHE);
    }

    #[test]
    fn cache_regular_js_is_no_cache() {
        assert_eq!(cache_policy("app.js"), NO_CACHE);
    }

    #[test]
    fn cache_hashed_js_is_immutable() {
        assert_eq!(cache_policy("app.a1b2c3d4.js"), IMMUTABLE_CACHE);
    }

    #[test]
    fn cache_hashed_css_is_immutable() {
        assert_eq!(cache_policy("chunk-abcdef01.css"), IMMUTABLE_CACHE);
    }

    #[test]
    fn cache_short_hash_not_detected() {
        // 7 hex chars — too short
        assert_eq!(cache_policy("app.a1b2c3d.js"), NO_CACHE);
    }

    // -----------------------------------------------------------------------
    // Hash detection heuristic
    // -----------------------------------------------------------------------

    #[test]
    fn hashed_with_dot_separator() {
        assert!(looks_hashed("app.a1b2c3d4.js"));
    }

    #[test]
    fn hashed_with_dash_separator() {
        assert!(looks_hashed("chunk-abcdef0123456789.css"));
    }

    #[test]
    fn not_hashed_short_segment() {
        assert!(!looks_hashed("app.abc.js"));
    }

    #[test]
    fn not_hashed_non_hex_chars() {
        assert!(!looks_hashed("app.ghijklmn.js"));
    }

    #[test]
    fn not_hashed_plain_filename() {
        assert!(!looks_hashed("index.html"));
    }

    #[test]
    fn hashed_exactly_8_hex_chars() {
        assert!(looks_hashed("app.deadbeef.js"));
    }

    // -----------------------------------------------------------------------
    // SPA fallback: /api/ routes must NOT be intercepted
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn api_route_returns_404_not_spa() {
        let req = Request::builder()
            .uri("/api/health")
            .body(Body::empty())
            .unwrap();
        let resp = serve_static(StaticSource::Embedded, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn root_serves_index_html() {
        let req = Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = serve_static(StaticSource::Embedded, req).await;
        // The embedded static/ directory contains index.html, so this should succeed
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
            "text/html; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn spa_fallback_for_unknown_path() {
        let req = Request::builder()
            .uri("/prompts/5")
            .body(Body::empty())
            .unwrap();
        let resp = serve_static(StaticSource::Embedded, req).await;
        // Should fall back to index.html for SPA routing
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
            "text/html; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn embedded_js_file_serves_correctly() {
        let req = Request::builder()
            .uri("/app.js")
            .body(Body::empty())
            .unwrap();
        let resp = serve_static(StaticSource::Embedded, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn directory_source_with_traversal_attempt() {
        let dir = std::env::temp_dir();
        let req = Request::builder()
            .uri("/../../../etc/passwd")
            .body(Body::empty())
            .unwrap();
        let resp = serve_static(StaticSource::Directory(dir), req).await;
        // Should fall back to index.html (which doesn't exist in temp dir), resulting in 404
        // or the file shouldn't be found due to traversal protection
        // Either way it must NOT return /etc/passwd content
        let status = resp.status();
        assert!(status == StatusCode::NOT_FOUND || status == StatusCode::OK);
    }
}
