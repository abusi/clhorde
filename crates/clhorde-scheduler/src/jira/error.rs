//! Error type for the Jira REST client.
//!
//! Hand-rolled (no `thiserror`) because the dep tree is already large
//! and the surface here is small. Each variant carries enough context
//! for `last_jira_error` rendering without leaking auth headers — the
//! `Display` impl never quotes request bodies or headers.

use std::fmt;

/// Errors surfaced by the Jira REST client.
#[derive(Debug)]
pub enum JiraError {
    /// Configured `auth_token_env` variable is unset or empty.
    EnvMissing { var: String },
    /// Configured `url` could not be parsed.
    InvalidUrl(String),
    /// Network-layer failure: DNS, TCP, TLS, connection refused, etc.
    /// This is the typical "Jira is down / firewalled" signal.
    Network(String),
    /// HTTP 401/403 — credentials rejected by the server.
    Unauthorized,
    /// HTTP 4xx other than 401/403 — request was bad and is not
    /// worth retrying. The body is included for diagnostics; callers
    /// should NOT log it at info level if they suspect it may echo
    /// authenticated content (it should not, but Jira has surprised us).
    Client { status: u16, body: String },
    /// HTTP 5xx persisting after retries.
    Server { status: u16, body: String },
    /// HTTP 429 persisting after retries.
    RateLimited { body: String },
    /// Response body was not the JSON shape we expected.
    Decode(String),
}

impl fmt::Display for JiraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EnvMissing { var } => write!(f, "Jira auth env var `{var}` is unset or empty"),
            Self::InvalidUrl(s) => write!(f, "invalid Jira URL: {s}"),
            Self::Network(s) => write!(f, "Jira network error: {s}"),
            Self::Unauthorized => f.write_str("Jira returned 401/403 — check auth_token_env"),
            Self::Client { status, .. } => write!(f, "Jira returned HTTP {status}"),
            Self::Server { status, .. } => {
                write!(f, "Jira returned HTTP {status} after retries")
            }
            Self::RateLimited { .. } => f.write_str("Jira rate-limited (429) after retries"),
            Self::Decode(s) => write!(f, "Jira response decode error: {s}"),
        }
    }
}

impl std::error::Error for JiraError {}
