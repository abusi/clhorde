//! Auth credentials for the Jira REST client.
//!
//! Atlassian Cloud expects HTTP Basic with `<email>:<api_token>`. The
//! token is read from an environment variable named in the source config
//! (`auth_token_env`). The wrapper here keeps the token off `Debug`
//! output and away from `Display`, so accidental `tracing::debug!` /
//! `error!` of the credentials never leaks the token.
//!
//! The plaintext token is only available through [`JiraAuth::header_value`],
//! which builds the encoded `Basic ...` header value the HTTP client
//! actually needs. Callers don't need the raw token themselves.

use std::env;
use std::fmt;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use super::error::JiraError;

/// HTTP Basic credentials for Atlassian Cloud.
///
/// `email` is informational and can be logged. `token` is wrapped in
/// [`SecretToken`] so tracing macros render it as `<redacted>` instead
/// of the actual API token.
#[derive(Clone)]
pub struct JiraAuth {
    pub email: String,
    token: SecretToken,
}

impl JiraAuth {
    /// Build credentials from raw values. Prefer
    /// [`JiraAuth::from_env`] in production code so the token never has
    /// to live in a config file.
    pub fn new(email: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            email: email.into(),
            token: SecretToken(token.into()),
        }
    }

    /// Read the token from the named environment variable. The `email`
    /// is passed in directly because it is not secret and is normally
    /// stored in `keymap.toml` next to the env var name.
    pub fn from_env(email: impl Into<String>, token_env: &str) -> Result<Self, JiraError> {
        let token = env::var(token_env).map_err(|_| JiraError::EnvMissing {
            var: token_env.to_string(),
        })?;
        if token.trim().is_empty() {
            return Err(JiraError::EnvMissing {
                var: token_env.to_string(),
            });
        }
        Ok(Self::new(email, token))
    }

    /// Pre-encoded `Authorization` header value (`Basic <b64>`).
    /// Computed on every call — this is not a hot path (one HTTP
    /// request at most every poll interval), and caching the encoded
    /// form would just be one more place to leak the token.
    pub fn header_value(&self) -> String {
        let raw = format!("{}:{}", self.email, self.token.0);
        format!("Basic {}", B64.encode(raw.as_bytes()))
    }
}

impl fmt::Debug for JiraAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JiraAuth")
            .field("email", &self.email)
            .field("token", &self.token)
            .finish()
    }
}

#[derive(Clone)]
struct SecretToken(String);

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"<redacted>\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_token() {
        let auth = JiraAuth::new("alice@example.com", "super-secret-token-1234");
        let dbg = format!("{auth:?}");
        assert!(dbg.contains("alice@example.com"));
        assert!(!dbg.contains("super-secret-token-1234"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn header_value_is_basic_b64() {
        let auth = JiraAuth::new("a@b.c", "tok");
        let h = auth.header_value();
        assert!(h.starts_with("Basic "));
        let b64 = h.strip_prefix("Basic ").unwrap();
        let decoded = B64.decode(b64).unwrap();
        assert_eq!(decoded, b"a@b.c:tok");
    }

    #[test]
    fn from_env_reports_missing_var() {
        // The env var "JIRA_TOKEN_DEFINITELY_UNSET_4242" should not exist.
        let var = "JIRA_TOKEN_DEFINITELY_UNSET_4242";
        std::env::remove_var(var);
        let err = JiraAuth::from_env("a@b.c", var).unwrap_err();
        assert!(matches!(err, JiraError::EnvMissing { .. }));
    }

    #[test]
    fn from_env_reads_token_value() {
        // Use a unique var name so this test is robust to env pollution
        // from sibling tests in the same process.
        let var = "JIRA_TOKEN_FROM_ENV_TEST_OK";
        std::env::set_var(var, "opaque-token");
        let auth = JiraAuth::from_env("a@b.c", var).unwrap();
        assert_eq!(auth.email, "a@b.c");
        assert!(auth.header_value().starts_with("Basic "));
        std::env::remove_var(var);
    }

    #[test]
    fn from_env_treats_empty_token_as_missing() {
        let var = "JIRA_TOKEN_EMPTY_TEST";
        std::env::set_var(var, "   ");
        let err = JiraAuth::from_env("a@b.c", var).unwrap_err();
        assert!(matches!(err, JiraError::EnvMissing { .. }));
        std::env::remove_var(var);
    }
}
