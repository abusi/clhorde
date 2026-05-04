//! Thin async Jira REST client.
//!
//! Section 3 of the `add-jira-source` change. The surface is intentionally
//! tiny — search, comment, transition, label add/remove — because the
//! poll loop in section 4 only needs those four primitives to drive the
//! lifecycle described in `design.md` (D4 polling, D8 write-back).
//!
//! ## Endpoints used
//! All requests go to Atlassian Cloud REST v2:
//! - `POST   /rest/api/2/search`                          (search by JQL)
//! - `POST   /rest/api/2/issue/{key}/comment`             (add comment)
//! - `GET    /rest/api/2/issue/{key}/transitions`         (list transitions)
//! - `POST   /rest/api/2/issue/{key}/transitions`         (apply transition)
//! - `PUT    /rest/api/2/issue/{key}`                     (label add/remove)
//!
//! v2 is used over v3 because comment bodies are plain text rather than
//! ADF — keeps the request shape one-line short and saves us writing an
//! ADF builder we'd otherwise only use to wrap "🤖 clhorde started…".
//!
//! ## Retry policy
//! [`BackoffPolicy`] drives an exponential backoff on 429s, 5xx, and
//! reqwest network errors. 4xx other than 429 short-circuits without
//! retrying — they're caller bugs (bad JQL, missing issue) and won't
//! get better with a second attempt. The policy is configurable so
//! tests can drive it with `base_delay_ms = 0` and skip the actual
//! sleeps.
//!
//! ## What this module does NOT do
//! - It does not own a `SourceHealth`. The poll loop (section 4) calls
//!   into the client, then updates [`crate::source::SourceHealth`] based
//!   on the `Result`. Keeping health out of the client makes it
//!   trivially mockable and isolates retry concerns from observability.
//! - It does not parse the trigger label or any per-queue config. The
//!   client takes a JQL string and returns issues; "what to do with
//!   them" is the source's job.

use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client as HttpClient, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use super::auth::JiraAuth;
use super::error::JiraError;
use super::event::JiraTicketPayload;

/// Exponential-backoff knobs for retryable failures (5xx, 429, network).
///
/// Defaults are tuned for production — five attempts with delays
/// growing 0.5s → 1s → 2s → 4s → 8s (cap 30s). Tests construct with
/// `base_delay_ms = 0` to drive the retry path without sleeping.
#[derive(Clone, Copy, Debug)]
pub struct BackoffPolicy {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    }
}

impl BackoffPolicy {
    /// No delay between attempts. For tests only — production must
    /// respect Jira's rate-limit and not hot-loop on 429.
    pub fn instant(max_retries: u32) -> Self {
        Self {
            max_retries,
            base_delay_ms: 0,
            max_delay_ms: 0,
        }
    }

    fn delay_for(&self, attempt: u32) -> Duration {
        if self.base_delay_ms == 0 {
            return Duration::ZERO;
        }
        let exp = self.base_delay_ms.saturating_mul(1u64 << attempt.min(20));
        Duration::from_millis(exp.min(self.max_delay_ms))
    }
}

/// Async Jira REST client. Cheap to clone — wraps a `reqwest::Client`
/// internally, which itself is cheap-clone.
#[derive(Clone, Debug)]
pub struct JiraClient {
    base_url: Url,
    auth: JiraAuth,
    http: HttpClient,
    backoff: BackoffPolicy,
}

impl JiraClient {
    /// Build a client with the default backoff policy and a sensible
    /// per-request timeout. Returns an error only if the URL or
    /// reqwest builder are invalid.
    pub fn new(base_url: &str, auth: JiraAuth) -> Result<Self, JiraError> {
        Self::with_backoff(base_url, auth, BackoffPolicy::default())
    }

    /// Build a client with an explicit backoff policy. Tests use
    /// [`BackoffPolicy::instant`] so retries don't add wall-clock time.
    pub fn with_backoff(
        base_url: &str,
        auth: JiraAuth,
        backoff: BackoffPolicy,
    ) -> Result<Self, JiraError> {
        let base_url = Url::parse(base_url).map_err(|e| JiraError::InvalidUrl(e.to_string()))?;
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| JiraError::Network(e.to_string()))?;
        Ok(Self {
            base_url,
            auth,
            http,
            backoff,
        })
    }

    /// Build a URL relative to `base_url`. Joining with leading-slash
    /// paths replaces the URL's path, so we strip it to make the
    /// `base_url + endpoint` concatenation behave intuitively when the
    /// caller's `base_url` already has a path component (e.g. mock
    /// servers using `http://127.0.0.1:N`).
    fn url(&self, endpoint: &str) -> Result<Url, JiraError> {
        let trimmed = endpoint.trim_start_matches('/');
        let mut base = self.base_url.clone();
        if !base.path().ends_with('/') {
            let new_path = format!("{}/", base.path());
            base.set_path(&new_path);
        }
        base.join(trimmed)
            .map_err(|e| JiraError::InvalidUrl(e.to_string()))
    }

    /// Execute one HTTP request with the configured retry policy.
    ///
    /// `build` is a closure rather than a single `RequestBuilder` so the
    /// retry loop can rebuild the request from scratch each attempt
    /// (`reqwest::RequestBuilder` is not `Clone` once a JSON body has
    /// been attached).
    async fn execute<F>(&self, build: F) -> Result<reqwest::Response, JiraError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempt: u32 = 0;
        loop {
            let req = build()
                .header(AUTHORIZATION, self.auth.header_value())
                .header(ACCEPT, "application/json");
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                        return Err(JiraError::Unauthorized);
                    }
                    let retryable =
                        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    if !retryable {
                        let body = resp.text().await.unwrap_or_default();
                        return Err(JiraError::Client {
                            status: status.as_u16(),
                            body,
                        });
                    }
                    if attempt >= self.backoff.max_retries {
                        let body = resp.text().await.unwrap_or_default();
                        return Err(if status == StatusCode::TOO_MANY_REQUESTS {
                            JiraError::RateLimited { body }
                        } else {
                            JiraError::Server {
                                status: status.as_u16(),
                                body,
                            }
                        });
                    }
                }
                Err(e) => {
                    if attempt >= self.backoff.max_retries {
                        return Err(JiraError::Network(e.to_string()));
                    }
                }
            }
            tokio::time::sleep(self.backoff.delay_for(attempt)).await;
            attempt += 1;
        }
    }

    /// Run a JQL search. Returns matching tickets projected into
    /// [`JiraTicketPayload`] — the shape the explore-gate template
    /// already consumes. `max_results` is forwarded to Jira (capped
    /// server-side at 100).
    pub async fn search_jql(
        &self,
        jql: &str,
        max_results: u32,
    ) -> Result<Vec<JiraTicketPayload>, JiraError> {
        let url = self.url("/rest/api/2/search")?;
        let body = json!({
            "jql": jql,
            "maxResults": max_results,
            "fields": ["summary", "description", "labels", "reporter", "customfield_10000"],
        });
        let resp = self
            .execute(|| self.http.post(url.clone()).header(CONTENT_TYPE, "application/json").json(&body))
            .await?;
        let raw: SearchResponse = resp
            .json()
            .await
            .map_err(|e| JiraError::Decode(e.to_string()))?;
        Ok(raw.issues.into_iter().map(payload_from_issue).collect())
    }

    /// Add a plain-text comment to an issue. v2 takes the body as a
    /// plain string; v3 would require ADF, which is overkill for the
    /// short status messages clhorde posts.
    pub async fn add_comment(&self, key: &str, body: &str) -> Result<(), JiraError> {
        let url = self.url(&format!("/rest/api/2/issue/{key}/comment"))?;
        let payload = json!({ "body": body });
        self.execute(|| {
            self.http
                .post(url.clone())
                .header(CONTENT_TYPE, "application/json")
                .json(&payload)
        })
        .await?;
        Ok(())
    }

    /// Apply a Jira transition. The caller is responsible for mapping
    /// workflow states to transition ids — this is per-Jira-project
    /// configuration that the source has visibility into via
    /// `[sources.jira.queues.<name>.transitions]`.
    pub async fn transition(&self, key: &str, transition_id: &str) -> Result<(), JiraError> {
        let url = self.url(&format!("/rest/api/2/issue/{key}/transitions"))?;
        let payload = json!({ "transition": { "id": transition_id } });
        self.execute(|| {
            self.http
                .post(url.clone())
                .header(CONTENT_TYPE, "application/json")
                .json(&payload)
        })
        .await?;
        Ok(())
    }

    /// Add a label to an issue. Implemented via the issue-update edit
    /// operations endpoint (`PUT /issue/{key}` with `update.labels`),
    /// which is idempotent server-side: re-adding an existing label is
    /// a no-op.
    pub async fn add_label(&self, key: &str, label: &str) -> Result<(), JiraError> {
        self.update_label(key, "add", label).await
    }

    /// Remove a label from an issue. Idempotent — removing an absent
    /// label is a no-op rather than a 404.
    pub async fn remove_label(&self, key: &str, label: &str) -> Result<(), JiraError> {
        self.update_label(key, "remove", label).await
    }

    async fn update_label(&self, key: &str, op: &str, label: &str) -> Result<(), JiraError> {
        let url = self.url(&format!("/rest/api/2/issue/{key}"))?;
        let payload = json!({
            "update": {
                "labels": [{ op: label }]
            }
        });
        self.execute(|| {
            self.http
                .put(url.clone())
                .header(CONTENT_TYPE, "application/json")
                .json(&payload)
        })
        .await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    issues: Vec<RawIssue>,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    key: String,
    #[serde(default)]
    fields: RawIssueFields,
}

#[derive(Debug, Default, Deserialize)]
struct RawIssueFields {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    reporter: Option<RawReporter>,
}

#[derive(Debug, Default, Deserialize)]
struct RawReporter {
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "accountId", default)]
    account_id: Option<String>,
}

fn payload_from_issue(issue: RawIssue) -> JiraTicketPayload {
    let RawIssue { key, fields } = issue;
    JiraTicketPayload {
        key: key.clone(),
        title: fields.summary.unwrap_or_default(),
        description: description_to_string(fields.description),
        // No standard Jira field for AC — source-side parsing of the
        // description is a section-4+ concern. Keep it empty here.
        acceptance_criteria: String::default(),
        labels: fields.labels,
        reporter: fields
            .reporter
            .and_then(|r| r.display_name.or(r.account_id)),
    }
}

/// Jira v2 returns description as either a plain string or `null`. v3
/// would return ADF (a JSON document); we tolerate both shapes by
/// flattening any non-string JSON to its `to_string()` form. Section
/// 4+ may want to render ADF properly — for now the explore prompt
/// just needs a blob of text.
fn description_to_string(v: Option<Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s,
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn auth() -> JiraAuth {
        JiraAuth::new("alice@example.com", "token-xyz")
    }

    fn instant_client(uri: &str) -> JiraClient {
        JiraClient::with_backoff(uri, auth(), BackoffPolicy::instant(3)).unwrap()
    }

    fn search_body() -> Value {
        json!({
            "issues": [
                {
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "Add OAuth",
                        "description": "Need oauth flow",
                        "labels": ["clhorde-plan", "auth"],
                        "reporter": { "displayName": "Alice", "accountId": "abc" }
                    }
                },
                {
                    "key": "PROJ-2",
                    "fields": {
                        "summary": "Other thing",
                        "description": null,
                        "labels": [],
                        "reporter": null
                    }
                }
            ]
        })
    }

    #[tokio::test]
    async fn search_success_returns_payloads() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .and(header(
                "authorization",
                auth().header_value().as_str(),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(search_body()))
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let issues = client.search_jql("project = PROJ", 50).await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].key, "PROJ-1");
        assert_eq!(issues[0].title, "Add OAuth");
        assert_eq!(issues[0].description, "Need oauth flow");
        assert_eq!(issues[0].labels, vec!["clhorde-plan", "auth"]);
        assert_eq!(issues[0].reporter.as_deref(), Some("Alice"));
        assert_eq!(issues[1].title, "Other thing");
        assert_eq!(issues[1].description, "");
        assert!(issues[1].reporter.is_none());
    }

    #[tokio::test]
    async fn unauthorized_does_not_retry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .expect(1) // exactly one call — no retries on 401
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let err = client
            .search_jql("project = PROJ", 50)
            .await
            .expect_err("401 should be an error");
        assert!(matches!(err, JiraError::Unauthorized), "got {err:?}");
    }

    #[tokio::test]
    async fn forbidden_is_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let client = instant_client(&server.uri());
        let err = client.search_jql("p = X", 1).await.unwrap_err();
        assert!(matches!(err, JiraError::Unauthorized));
    }

    /// A `Respond` impl that fails the first N requests with the given
    /// status, then returns a 200 with `body` for the remainder.
    struct FailThenOk {
        fail_status: u16,
        fail_count: usize,
        seen: Arc<AtomicUsize>,
        body: Value,
    }

    impl Respond for FailThenOk {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n = self.seen.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_count {
                ResponseTemplate::new(self.fail_status).set_body_string("transient")
            } else {
                ResponseTemplate::new(200).set_body_json(self.body.clone())
            }
        }
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let server = MockServer::start().await;
        let seen = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(FailThenOk {
                fail_status: 429,
                fail_count: 2,
                seen: seen.clone(),
                body: search_body(),
            })
            .expect(3) // 2 failures + 1 success
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let issues = client.search_jql("p = PROJ", 50).await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        let seen = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(FailThenOk {
                fail_status: 500,
                fail_count: 1,
                seen: seen.clone(),
                body: search_body(),
            })
            .expect(2)
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let issues = client.search_jql("p = PROJ", 50).await.unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(4) // initial + 3 retries
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let err = client.search_jql("p = PROJ", 50).await.unwrap_err();
        assert!(matches!(err, JiraError::Server { status: 500, .. }), "{err:?}");
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_on_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .expect(4)
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let err = client.search_jql("p = PROJ", 50).await.unwrap_err();
        assert!(matches!(err, JiraError::RateLimited { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn client_4xx_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/search"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad jql"))
            .expect(1)
            .mount(&server)
            .await;

        let client = instant_client(&server.uri());
        let err = client.search_jql("???", 50).await.unwrap_err();
        match err {
            JiraError::Client { status, .. } => assert_eq!(status, 400),
            other => panic!("expected Client error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn offline_returns_network_error() {
        // No mock server — bind to an unused localhost port. 127.0.0.1:1
        // is the historical "definitely-nothing-here" port; on Linux it
        // refuses immediately.
        let client = instant_client("http://127.0.0.1:1");
        let err = client.search_jql("p = PROJ", 50).await.unwrap_err();
        assert!(matches!(err, JiraError::Network(_)), "{err:?}");
    }

    #[tokio::test]
    async fn add_comment_posts_body_to_correct_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/issue/PROJ-1/comment"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let client = instant_client(&server.uri());
        client.add_comment("PROJ-1", "🤖 hello").await.unwrap();
    }

    #[tokio::test]
    async fn transition_posts_to_transitions_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/api/2/issue/PROJ-1/transitions"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = instant_client(&server.uri());
        client.transition("PROJ-1", "31").await.unwrap();
    }

    #[tokio::test]
    async fn add_label_puts_to_issue_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/2/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = instant_client(&server.uri());
        client.add_label("PROJ-1", "clhorde-plan").await.unwrap();
    }

    #[tokio::test]
    async fn remove_label_puts_to_issue_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/rest/api/2/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = instant_client(&server.uri());
        client.remove_label("PROJ-1", "clhorde-plan").await.unwrap();
    }

    #[test]
    fn backoff_grows_exponentially() {
        let p = BackoffPolicy {
            max_retries: 5,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
        };
        assert_eq!(p.delay_for(0), Duration::from_millis(100));
        assert_eq!(p.delay_for(1), Duration::from_millis(200));
        assert_eq!(p.delay_for(2), Duration::from_millis(400));
        assert_eq!(p.delay_for(3), Duration::from_millis(800));
        // Capped at max_delay_ms.
        assert_eq!(p.delay_for(20), Duration::from_millis(5_000));
    }

    #[test]
    fn instant_backoff_skips_sleep() {
        let p = BackoffPolicy::instant(3);
        for n in 0..10 {
            assert_eq!(p.delay_for(n), Duration::ZERO);
        }
    }
}


