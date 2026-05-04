//! Validate and convert the parsed `[sources.jira]` TOML schema into the
//! runtime types the source needs.
//!
//! Section 8 of the `add-jira-source` change. The schema itself lives in
//! [`clhorde_core::keymap::TomlSourcesJira`] (the parse layer); this
//! module owns the strongly-typed runtime view and the validation rules
//! that run at scheduler startup.
//!
//! ## What validation does
//!
//! - Enforces the source-wide required fields (`url`, `auth_token_env`,
//!   `email`).
//! - Clamps `poll_interval_secs` to the [`MIN_POLL_INTERVAL`] floor,
//!   carrying the original value forward as a "was clamped" signal so
//!   the daemon can log a one-shot warning at startup.
//! - Defaults `max_concurrent_explore` and `idle_explore_timeout` from
//!   the constants in [`super::source`] / [`crate::explore`].
//! - Rejects queues whose `mode = "direct"` with a clear "not yet
//!   implemented" error pointing to the follow-up change. Queues with
//!   `mode = "openspec"` (the default) continue to register.
//! - Rejects unrecognised mode values (anything other than the two
//!   above) with an error naming the offending queue.
//! - Resolves transition keys (`"exploring"`, `"archived"`,
//!   `"cancelled"`) into the strongly-typed [`LifecyclePhase`] map used
//!   by [`super::writeback::JiraWritebackConfig`].
//! - Refuses queue names that would be unsafe as a filename component
//!   (containing `/`, `\`, NUL, or `..`) — the source's persistence
//!   layer (see [`super::source::JiraSourceStore`]) drops the queue name
//!   into the path verbatim, so this is the right place to enforce it.
//!
//! ## What validation does NOT do
//!
//! - Does NOT contact Jira. Auth-related failures (token env missing or
//!   empty, network unreachable) are surfaced through `SourceHealth` at
//!   runtime, not at config-validation time.
//! - Does NOT create directories on disk. The store is opened lazily.

use std::collections::BTreeMap;
use std::time::Duration;

use clhorde_core::keymap::{TomlSourcesJira, TomlSourcesJiraQueue};

use crate::explore::DEFAULT_IDLE_THRESHOLD;

use super::source::{
    JiraSourceConfig, QueueConfig, DEFAULT_MAX_CONCURRENT_EXPLORE, DEFAULT_POLL_INTERVAL,
    MIN_POLL_INTERVAL,
};
use super::writeback::{
    JiraWritebackConfig, LifecyclePhase, COMMENT_ARCHIVED, COMMENT_CANCELLED, COMMENT_EXPLORING,
    DEFAULT_TRIGGER_LABEL,
};

/// Queue mode: routes through the explore gate.
pub const MODE_OPENSPEC: &str = "openspec";

/// Reserved queue mode: a single-prompt path that bypasses the explore
/// gate. Wired through config but rejected at startup until the
/// follow-up change implements the runtime.
pub const MODE_DIRECT: &str = "direct";

/// Validation failure when building a [`JiraConfig`] from
/// [`TomlSourcesJira`].
///
/// `Display` impl is the message logged at scheduler startup; the
/// daemon binary refuses to register a queue (or the whole source)
/// based on which variant lands.
#[derive(Debug, PartialEq, Eq)]
pub enum JiraConfigError {
    /// `url` was missing from `[sources.jira]`.
    MissingUrl,
    /// `email` was missing from `[sources.jira]`.
    MissingEmail,
    /// `auth_token_env` was missing from `[sources.jira]`.
    MissingAuthTokenEnv,
    /// A queue's `filter_jql` was missing.
    MissingFilterJql { queue: String },
    /// A queue used `mode = "direct"`, which is reserved but not yet
    /// implemented in this change.
    DirectModeNotImplemented { queue: String },
    /// A queue used a `mode` value that's not one of the recognised
    /// strings.
    UnrecognisedMode { queue: String, mode: String },
    /// A queue's `transitions` table referenced a phase name we don't
    /// understand. Accepted phase names are `"exploring"`, `"archived"`,
    /// `"cancelled"` (case-insensitive).
    UnrecognisedTransitionPhase { queue: String, phase: String },
    /// A queue name contained characters that would be unsafe in a
    /// filesystem path component (`/`, `\`, NUL, or the literal `..`).
    UnsafeQueueName { queue: String },
    /// `max_concurrent_explore` was set to zero. A zero cap would queue
    /// every ticket forever — almost certainly a misconfiguration.
    ZeroMaxConcurrentExplore,
}

impl std::fmt::Display for JiraConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingUrl => f.write_str(
                "[sources.jira] missing required field `url` \
                 (e.g. `url = \"https://your-site.atlassian.net\"`)",
            ),
            Self::MissingEmail => f.write_str(
                "[sources.jira] missing required field `email` \
                 (the Atlassian account tied to the API token)",
            ),
            Self::MissingAuthTokenEnv => f.write_str(
                "[sources.jira] missing required field `auth_token_env` \
                 (name of the env var holding the Atlassian API token)",
            ),
            Self::MissingFilterJql { queue } => write!(
                f,
                "[sources.jira.queues.{queue}] missing required field `filter_jql`",
            ),
            Self::DirectModeNotImplemented { queue } => write!(
                f,
                "[sources.jira.queues.{queue}] mode = \"direct\" is reserved but \
                 not yet implemented in this change; track the follow-up change \
                 or use mode = \"openspec\" (the default)",
            ),
            Self::UnrecognisedMode { queue, mode } => write!(
                f,
                "[sources.jira.queues.{queue}] unrecognised mode {mode:?}; \
                 expected \"openspec\" or \"direct\"",
            ),
            Self::UnrecognisedTransitionPhase { queue, phase } => write!(
                f,
                "[sources.jira.queues.{queue}.transitions] unrecognised phase \
                 {phase:?}; expected \"exploring\", \"archived\", or \"cancelled\"",
            ),
            Self::UnsafeQueueName { queue } => write!(
                f,
                "[sources.jira.queues.{queue}] queue name contains unsafe \
                 characters (`/`, `\\`, NUL, or `..`); pick a name suitable \
                 for a filename component",
            ),
            Self::ZeroMaxConcurrentExplore => f.write_str(
                "[sources.jira] max_concurrent_explore must be >= 1 \
                 (zero would queue every ticket forever)",
            ),
        }
    }
}

impl std::error::Error for JiraConfigError {}

/// Validated runtime view of `[sources.jira]`.
///
/// The daemon binary holds one of these and uses it to:
/// 1. Build the [`JiraAuth`] (via [`Self::auth_token_env`] +
///    [`Self::email`]).
/// 2. Build the [`JiraClient`] (via [`Self::url`]).
/// 3. Spawn the poll loop with [`Self::source_config`].
/// 4. Spawn the reaper with [`Self::idle_explore_timeout`].
/// 5. Build per-queue [`JiraWritebackConfig`] entries via
///    [`Self::writeback_for`].
///
/// [`JiraAuth`]: crate::jira::JiraAuth
/// [`JiraClient`]: crate::jira::JiraClient
#[derive(Debug, Clone)]
pub struct JiraConfig {
    /// Atlassian site base URL.
    pub url: String,
    /// Account email used as the HTTP Basic username half.
    pub email: String,
    /// Env var name to read the API token from at request time.
    pub auth_token_env: String,
    /// Source-wide poll cadence (already clamped to
    /// [`MIN_POLL_INTERVAL`] by [`build`]).
    pub poll_interval: Duration,
    /// Whether the original `poll_interval_secs` got clamped to the
    /// floor. Used by the daemon to log a one-shot warning at startup.
    pub poll_interval_clamped: bool,
    /// Per-source explore-worker concurrency cap.
    pub max_concurrent_explore: usize,
    /// Idle threshold after which the reaper kills explore workers.
    pub idle_explore_timeout: Duration,
    /// All queues that survived validation. Each entry pairs the parsed
    /// [`QueueConfig`] (filter + name) with its [`JiraWritebackConfig`]
    /// (comments / labels / transitions / trigger label). Ordered by
    /// queue name so test assertions are stable.
    pub queues: Vec<JiraQueue>,
}

/// One validated queue entry.
#[derive(Debug, Clone)]
pub struct JiraQueue {
    /// Name + JQL forwarded to the poll loop.
    pub config: QueueConfig,
    /// Per-queue write-back behaviour.
    pub writeback: JiraWritebackConfig,
}

impl JiraConfig {
    /// Just the source-wide fields the poll loop needs, without the
    /// per-queue write-back state. The poll loop only cares about the
    /// JQL filter set; the orchestrator is what consults the per-queue
    /// write-back config when a workflow advances.
    pub fn source_config(&self) -> JiraSourceConfig {
        JiraSourceConfig::new(
            self.poll_interval,
            self.queues.iter().map(|q| q.config.clone()).collect(),
        )
    }

    /// Look up the write-back config for a queue by name. Returns
    /// `None` if no queue with that name was registered.
    pub fn writeback_for(&self, queue_name: &str) -> Option<&JiraWritebackConfig> {
        self.queues
            .iter()
            .find(|q| q.config.name == queue_name)
            .map(|q| &q.writeback)
    }
}

/// Outcome of [`build_partial`]: the source-wide config plus any
/// per-queue errors that did NOT block the whole source from
/// registering. Per the spec a single misconfigured queue (e.g. one
/// that uses `mode = "direct"`) should not stop other queues in the
/// same source from working.
#[derive(Debug)]
pub struct ValidationOutcome {
    /// The validated source-wide config plus only the queues that
    /// passed validation.
    pub config: JiraConfig,
    /// Errors for queues that were skipped. Empty when every queue
    /// validated cleanly.
    pub skipped_queue_errors: Vec<JiraConfigError>,
}

/// Validate and build a [`JiraConfig`] from the parsed
/// `[sources.jira]` block, with strict semantics: any error (source-wide
/// or per-queue) fails the whole call.
///
/// Use [`build_partial`] in production startup paths; `build` is
/// retained for tests and tooling that wants every error surfaced.
pub fn build(toml: &TomlSourcesJira) -> Result<JiraConfig, Vec<JiraConfigError>> {
    let mut errors = Vec::new();

    // Source-wide required fields.
    let url = toml
        .url
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let email = toml
        .email
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let auth_token_env = toml
        .auth_token_env
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if url.is_none() {
        errors.push(JiraConfigError::MissingUrl);
    }
    if email.is_none() {
        errors.push(JiraConfigError::MissingEmail);
    }
    if auth_token_env.is_none() {
        errors.push(JiraConfigError::MissingAuthTokenEnv);
    }

    // Source-wide numeric defaults.
    let requested_poll = toml
        .poll_interval_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL);
    let poll_interval_clamped = JiraSourceConfig::was_clamped(requested_poll);
    let poll_interval = requested_poll.max(MIN_POLL_INTERVAL);

    let max_concurrent_explore = toml
        .max_concurrent_explore
        .unwrap_or(DEFAULT_MAX_CONCURRENT_EXPLORE);
    if max_concurrent_explore == 0 {
        errors.push(JiraConfigError::ZeroMaxConcurrentExplore);
    }

    let idle_explore_timeout = toml
        .idle_explore_timeout
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_THRESHOLD);

    // Per-queue validation. Iterate in sorted order so error messages
    // and the final queue list are deterministic.
    let mut queue_names: Vec<&String> = toml.queues.keys().collect();
    queue_names.sort();
    let mut queues: Vec<JiraQueue> = Vec::with_capacity(queue_names.len());
    for name in queue_names {
        match build_queue(name, &toml.queues[name]) {
            Ok(q) => queues.push(q),
            Err(mut e) => errors.append(&mut e),
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(JiraConfig {
        // Unwrap-safe: the absence of any of these would have pushed
        // onto `errors` and triggered the early return above.
        url: url.unwrap(),
        email: email.unwrap(),
        auth_token_env: auth_token_env.unwrap(),
        poll_interval,
        poll_interval_clamped,
        max_concurrent_explore,
        idle_explore_timeout,
        queues,
    })
}

/// Validate and build a [`JiraConfig`], allowing individual queues to
/// be skipped without disabling the whole source.
///
/// - Source-wide errors (`MissingUrl`, `MissingEmail`,
///   `MissingAuthTokenEnv`, `ZeroMaxConcurrentExplore`) hard-fail and
///   return `Err` — without them the source can't function at all.
/// - Per-queue errors (`DirectModeNotImplemented`, `UnrecognisedMode`,
///   `MissingFilterJql`, `UnrecognisedTransitionPhase`,
///   `UnsafeQueueName`) cause the offending queue to be skipped; the
///   source is still registered and the errors land in
///   [`ValidationOutcome::skipped_queue_errors`] for the caller to log.
///
/// This matches the spec scenario "Direct mode rejected at startup":
/// the offending queue is skipped with a clear error pointing to the
/// follow-up change, but other `openspec` queues in the same source
/// continue to work.
pub fn build_partial(
    toml: &TomlSourcesJira,
) -> Result<ValidationOutcome, Vec<JiraConfigError>> {
    let mut source_errors = Vec::new();

    let url = toml
        .url
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let email = toml
        .email
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let auth_token_env = toml
        .auth_token_env
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    if url.is_none() {
        source_errors.push(JiraConfigError::MissingUrl);
    }
    if email.is_none() {
        source_errors.push(JiraConfigError::MissingEmail);
    }
    if auth_token_env.is_none() {
        source_errors.push(JiraConfigError::MissingAuthTokenEnv);
    }

    let requested_poll = toml
        .poll_interval_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL);
    let poll_interval_clamped = JiraSourceConfig::was_clamped(requested_poll);
    let poll_interval = requested_poll.max(MIN_POLL_INTERVAL);

    let max_concurrent_explore = toml
        .max_concurrent_explore
        .unwrap_or(DEFAULT_MAX_CONCURRENT_EXPLORE);
    if max_concurrent_explore == 0 {
        source_errors.push(JiraConfigError::ZeroMaxConcurrentExplore);
    }

    let idle_explore_timeout = toml
        .idle_explore_timeout
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_THRESHOLD);

    if !source_errors.is_empty() {
        return Err(source_errors);
    }

    let mut queue_names: Vec<&String> = toml.queues.keys().collect();
    queue_names.sort();
    let mut queues: Vec<JiraQueue> = Vec::with_capacity(queue_names.len());
    let mut skipped: Vec<JiraConfigError> = Vec::new();
    for name in queue_names {
        match build_queue(name, &toml.queues[name]) {
            Ok(q) => queues.push(q),
            Err(errs) => {
                // Each queue's errors land in the skip list; the
                // queue itself is dropped from the registered set.
                skipped.extend(errs);
            }
        }
    }

    Ok(ValidationOutcome {
        config: JiraConfig {
            // Unwraps safe: source_errors empty implies all required
            // fields populated.
            url: url.unwrap(),
            email: email.unwrap(),
            auth_token_env: auth_token_env.unwrap(),
            poll_interval,
            poll_interval_clamped,
            max_concurrent_explore,
            idle_explore_timeout,
            queues,
        },
        skipped_queue_errors: skipped,
    })
}

fn build_queue(
    name: &str,
    queue: &TomlSourcesJiraQueue,
) -> Result<JiraQueue, Vec<JiraConfigError>> {
    let mut errors = Vec::new();

    if !is_safe_queue_name(name) {
        errors.push(JiraConfigError::UnsafeQueueName {
            queue: name.to_string(),
        });
    }

    let filter_jql = queue
        .filter_jql
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    if filter_jql.is_none() {
        errors.push(JiraConfigError::MissingFilterJql {
            queue: name.to_string(),
        });
    }

    let mode = queue
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(MODE_OPENSPEC);
    match mode {
        MODE_OPENSPEC => {}
        MODE_DIRECT => {
            errors.push(JiraConfigError::DirectModeNotImplemented {
                queue: name.to_string(),
            });
        }
        other => {
            errors.push(JiraConfigError::UnrecognisedMode {
                queue: name.to_string(),
                mode: other.to_string(),
            });
        }
    }

    let mut transitions: BTreeMap<LifecyclePhase, String> = BTreeMap::new();
    for (phase_str, transition_id) in &queue.transitions {
        match parse_phase(phase_str) {
            Some(phase) => {
                transitions.insert(phase, transition_id.trim().to_string());
            }
            None => {
                errors.push(JiraConfigError::UnrecognisedTransitionPhase {
                    queue: name.to_string(),
                    phase: phase_str.clone(),
                });
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let writeback = JiraWritebackConfig {
        comments: queue.comments.unwrap_or(true),
        labels: queue.labels.unwrap_or(true),
        trigger_label: queue
            .trigger_label
            .clone()
            .unwrap_or_else(|| DEFAULT_TRIGGER_LABEL.to_string()),
        transitions,
        comment_exploring: COMMENT_EXPLORING.to_string(),
        comment_archived: COMMENT_ARCHIVED.to_string(),
        comment_cancelled: COMMENT_CANCELLED.to_string(),
    };

    Ok(JiraQueue {
        config: QueueConfig {
            name: name.to_string(),
            // Unwrap-safe: covered by the missing-field check above.
            filter_jql: filter_jql.unwrap(),
        },
        writeback,
    })
}

fn parse_phase(s: &str) -> Option<LifecyclePhase> {
    match s.trim().to_ascii_lowercase().as_str() {
        "exploring" => Some(LifecyclePhase::Exploring),
        "archived" => Some(LifecyclePhase::Archived),
        "cancelled" | "canceled" => Some(LifecyclePhase::Cancelled),
        _ => None,
    }
}

/// Queue names land in filesystem paths verbatim (see
/// [`super::source::JiraSourceStore::file_path`]). The check is
/// intentionally conservative: TOML lets you put almost anything in a
/// table key, including spaces and unicode, but we want to make sure
/// callers can't accidentally escape the data dir.
fn is_safe_queue_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name == ".." || name == "." {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(toml_str: &str) -> TomlSourcesJira {
        // Round-trip through clhorde-core's full TomlConfig so the test
        // exercises the actual parse path.
        let cfg: clhorde_core::keymap::TomlConfig = toml::from_str(toml_str).unwrap();
        cfg.sources.unwrap().jira.unwrap()
    }

    #[test]
    fn round_trip_full_config() {
        let toml_str = r#"
[sources.jira]
url = "https://example.atlassian.net"
email = "bot@example.com"
auth_token_env = "JIRA_TOKEN"
poll_interval_secs = 60
max_concurrent_explore = 3
idle_explore_timeout = 7200

[sources.jira.queues.backlog]
filter_jql = "project = PROJ AND labels = clhorde-plan"
mode = "openspec"
comments = true
labels = true

[sources.jira.queues.backlog.transitions]
exploring = "31"
archived = "61"

[sources.jira.queues.urgent]
filter_jql = "priority = Highest AND labels = clhorde-plan"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).expect("config valid");

        assert_eq!(cfg.url, "https://example.atlassian.net");
        assert_eq!(cfg.email, "bot@example.com");
        assert_eq!(cfg.auth_token_env, "JIRA_TOKEN");
        assert_eq!(cfg.poll_interval, Duration::from_secs(60));
        assert!(!cfg.poll_interval_clamped);
        assert_eq!(cfg.max_concurrent_explore, 3);
        assert_eq!(cfg.idle_explore_timeout, Duration::from_secs(7200));

        // Queues sorted alphabetically.
        assert_eq!(cfg.queues.len(), 2);
        assert_eq!(cfg.queues[0].config.name, "backlog");
        assert_eq!(
            cfg.queues[0].config.filter_jql,
            "project = PROJ AND labels = clhorde-plan"
        );
        assert!(cfg.queues[0].writeback.comments);
        assert!(cfg.queues[0].writeback.labels);
        assert_eq!(cfg.queues[0].writeback.trigger_label, DEFAULT_TRIGGER_LABEL);
        assert_eq!(
            cfg.queues[0].writeback.transitions.get(&LifecyclePhase::Exploring),
            Some(&"31".to_string()),
        );
        assert_eq!(
            cfg.queues[0].writeback.transitions.get(&LifecyclePhase::Archived),
            Some(&"61".to_string()),
        );

        assert_eq!(cfg.queues[1].config.name, "urgent");
        // Defaults applied even when omitted.
        assert!(cfg.queues[1].writeback.comments);
        assert!(cfg.queues[1].writeback.labels);
        assert!(cfg.queues[1].writeback.transitions.is_empty());
    }

    #[test]
    fn defaults_applied_when_optional_fields_omitted() {
        let toml_str = r#"
[sources.jira]
url = "https://example.atlassian.net"
email = "bot@example.com"
auth_token_env = "JIRA_TOKEN"

[sources.jira.queues.backlog]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).expect("defaults must produce a valid config");

        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
        assert!(!cfg.poll_interval_clamped);
        assert_eq!(cfg.max_concurrent_explore, DEFAULT_MAX_CONCURRENT_EXPLORE);
        assert_eq!(cfg.idle_explore_timeout, DEFAULT_IDLE_THRESHOLD);

        let q = &cfg.queues[0];
        assert!(q.writeback.comments, "comments default on");
        assert!(q.writeback.labels, "labels default on");
        assert_eq!(q.writeback.trigger_label, DEFAULT_TRIGGER_LABEL);
        assert!(q.writeback.transitions.is_empty(), "transitions default off");
    }

    #[test]
    fn poll_interval_below_floor_is_clamped() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"
poll_interval_secs = 5

[sources.jira.queues.backlog]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).expect("config valid even when clamped");
        assert_eq!(cfg.poll_interval, MIN_POLL_INTERVAL);
        assert!(cfg.poll_interval_clamped);
    }

    #[test]
    fn missing_url_is_an_error() {
        let toml = TomlSourcesJira {
            url: None,
            email: Some("a@b.c".into()),
            auth_token_env: Some("TOK".into()),
            poll_interval_secs: None,
            max_concurrent_explore: None,
            idle_explore_timeout: None,
            queues: HashMap::new(),
        };
        let errs = build(&toml).expect_err("missing url must fail");
        assert!(errs.contains(&JiraConfigError::MissingUrl), "got {errs:?}");
    }

    #[test]
    fn missing_email_is_an_error() {
        let toml = TomlSourcesJira {
            url: Some("https://x.atlassian.net".into()),
            email: None,
            auth_token_env: Some("TOK".into()),
            poll_interval_secs: None,
            max_concurrent_explore: None,
            idle_explore_timeout: None,
            queues: HashMap::new(),
        };
        let errs = build(&toml).expect_err("missing email must fail");
        assert!(errs.contains(&JiraConfigError::MissingEmail), "got {errs:?}");
    }

    #[test]
    fn missing_auth_token_env_is_an_error() {
        let toml = TomlSourcesJira {
            url: Some("https://x.atlassian.net".into()),
            email: Some("a@b.c".into()),
            auth_token_env: None,
            poll_interval_secs: None,
            max_concurrent_explore: None,
            idle_explore_timeout: None,
            queues: HashMap::new(),
        };
        let errs = build(&toml).expect_err("missing auth_token_env must fail");
        assert!(
            errs.contains(&JiraConfigError::MissingAuthTokenEnv),
            "got {errs:?}",
        );
    }

    #[test]
    fn whitespace_only_required_fields_are_treated_as_missing() {
        let toml_str = r#"
[sources.jira]
url = "   "
email = ""
auth_token_env = "\n"
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("whitespace-only fields must fail");
        assert!(errs.contains(&JiraConfigError::MissingUrl));
        assert!(errs.contains(&JiraConfigError::MissingEmail));
        assert!(errs.contains(&JiraConfigError::MissingAuthTokenEnv));
    }

    #[test]
    fn missing_filter_jql_is_an_error() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.backlog]
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("queue without filter_jql must fail");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                JiraConfigError::MissingFilterJql { queue } if queue == "backlog"
            )),
            "got {errs:?}",
        );
    }

    #[test]
    fn direct_mode_is_rejected_with_clear_pointer_to_followup() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"
mode = "direct"
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("direct mode must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                JiraConfigError::DirectModeNotImplemented { queue } if queue == "foo"
            )),
            "got {errs:?}",
        );
        // Display message points to the follow-up change.
        let msg = format!("{}", errs[0]);
        assert!(
            msg.contains("not yet implemented"),
            "error message should mention follow-up change: {msg}",
        );
    }

    #[test]
    fn unrecognised_mode_is_rejected() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"
mode = "weird"
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("unknown mode must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                JiraConfigError::UnrecognisedMode { queue, mode }
                    if queue == "foo" && mode == "weird"
            )),
            "got {errs:?}",
        );
    }

    #[test]
    fn openspec_mode_is_the_default() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).expect("openspec is the default");
        assert_eq!(cfg.queues.len(), 1);
        assert_eq!(cfg.queues[0].config.name, "foo");
    }

    #[test]
    fn unrecognised_transition_phase_is_rejected() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"

[sources.jira.queues.foo.transitions]
unknown_phase = "31"
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("unknown transition phase must fail");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                JiraConfigError::UnrecognisedTransitionPhase { queue, phase }
                    if queue == "foo" && phase == "unknown_phase"
            )),
            "got {errs:?}",
        );
    }

    #[test]
    fn transition_phase_lookup_is_case_insensitive_and_accepts_canceled_alias() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"

[sources.jira.queues.foo.transitions]
EXPLORING = "11"
Archived = "21"
canceled = "31"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).expect("case-insensitive phase names are accepted");
        let t = &cfg.queues[0].writeback.transitions;
        assert_eq!(t.get(&LifecyclePhase::Exploring), Some(&"11".to_string()));
        assert_eq!(t.get(&LifecyclePhase::Archived), Some(&"21".to_string()));
        assert_eq!(t.get(&LifecyclePhase::Cancelled), Some(&"31".to_string()));
    }

    #[test]
    fn zero_max_concurrent_explore_is_rejected() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"
max_concurrent_explore = 0
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("max_concurrent_explore = 0 must fail");
        assert!(
            errs.contains(&JiraConfigError::ZeroMaxConcurrentExplore),
            "got {errs:?}",
        );
    }

    #[test]
    fn unsafe_queue_name_is_rejected() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues."../escape"]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("path-traversal queue name must fail");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                JiraConfigError::UnsafeQueueName { queue } if queue == "../escape"
            )),
            "got {errs:?}",
        );
    }

    #[test]
    fn queues_are_sorted_for_deterministic_output() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.zeta]
filter_jql = "z"

[sources.jira.queues.alpha]
filter_jql = "a"

[sources.jira.queues.middle]
filter_jql = "m"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).unwrap();
        let names: Vec<_> = cfg.queues.iter().map(|q| q.config.name.clone()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zeta"]);
    }

    #[test]
    fn source_config_carries_clamped_interval_and_queue_set() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"
poll_interval_secs = 1

[sources.jira.queues.backlog]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).unwrap();
        let src = cfg.source_config();
        assert_eq!(src.poll_interval, MIN_POLL_INTERVAL);
        assert_eq!(src.queues.len(), 1);
        assert_eq!(src.queues[0].name, "backlog");
    }

    #[test]
    fn writeback_for_returns_per_queue_config() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.backlog]
filter_jql = "project = PROJ"
comments = false

[sources.jira.queues.urgent]
filter_jql = "priority = Highest"
labels = false
"#;
        let parsed = parse(toml_str);
        let cfg = build(&parsed).unwrap();
        let backlog = cfg.writeback_for("backlog").unwrap();
        let urgent = cfg.writeback_for("urgent").unwrap();
        assert!(!backlog.comments);
        assert!(backlog.labels);
        assert!(urgent.comments);
        assert!(!urgent.labels);
        assert!(cfg.writeback_for("nope").is_none());
    }

    #[test]
    fn omitted_jira_block_is_not_a_validation_target() {
        // Sanity: when the user omits [sources.jira] entirely,
        // `clhorde_core::keymap::TomlConfig::sources` is `None` and the
        // daemon never calls `build`. This test documents that
        // contract: building from `Default::default()` succeeds for the
        // numeric defaults but fails the required-fields gate, which is
        // exactly what we want — the caller is responsible for skipping
        // `build` when the block is absent.
        let toml = TomlSourcesJira::default();
        let errs = build(&toml).expect_err("default block has no required fields");
        assert!(errs.contains(&JiraConfigError::MissingUrl));
        assert!(errs.contains(&JiraConfigError::MissingEmail));
        assert!(errs.contains(&JiraConfigError::MissingAuthTokenEnv));
    }

    #[test]
    fn partial_skips_direct_queue_but_keeps_openspec_queues() {
        // Spec scenario `jira-source` Requirement 6 / "Direct mode
        // rejected at startup": the direct-mode queue is skipped with
        // a clear error pointing to the follow-up change, but the
        // openspec queue in the same source continues to work.
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"
mode = "direct"

[sources.jira.queues.bar]
filter_jql = "project = OTHER"
mode = "openspec"
"#;
        let parsed = parse(toml_str);
        let outcome = build_partial(&parsed).expect("source-level validation passes");
        let names: Vec<_> = outcome
            .config
            .queues
            .iter()
            .map(|q| q.config.name.clone())
            .collect();
        assert_eq!(names, vec!["bar"], "direct-mode queue must be dropped");
        assert!(
            outcome
                .skipped_queue_errors
                .iter()
                .any(|e| matches!(
                    e,
                    JiraConfigError::DirectModeNotImplemented { queue } if queue == "foo"
                )),
            "skipped errors must surface direct-mode rejection: {:?}",
            outcome.skipped_queue_errors,
        );
    }

    #[test]
    fn partial_hard_fails_on_missing_source_required_fields() {
        let toml_str = r#"
[sources.jira]
poll_interval_secs = 60

[sources.jira.queues.foo]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let errs = build_partial(&parsed).expect_err("missing url/email/token must hard-fail");
        assert!(errs.contains(&JiraConfigError::MissingUrl));
        assert!(errs.contains(&JiraConfigError::MissingEmail));
        assert!(errs.contains(&JiraConfigError::MissingAuthTokenEnv));
    }

    #[test]
    fn partial_returns_clean_outcome_when_every_queue_passes() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.bar]
filter_jql = "project = PROJ"
"#;
        let parsed = parse(toml_str);
        let outcome = build_partial(&parsed).unwrap();
        assert_eq!(outcome.config.queues.len(), 1);
        assert!(outcome.skipped_queue_errors.is_empty());
    }

    #[test]
    fn partial_aggregates_multiple_queue_errors_per_queue() {
        // A single queue can fail multiple validation steps (e.g.
        // missing filter_jql AND unrecognised mode). All of its
        // errors should land in `skipped_queue_errors`.
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
mode = "weird"
"#;
        let parsed = parse(toml_str);
        let outcome = build_partial(&parsed).unwrap();
        assert!(outcome.config.queues.is_empty());
        assert!(
            outcome
                .skipped_queue_errors
                .iter()
                .any(|e| matches!(e, JiraConfigError::MissingFilterJql { .. })),
            "got {:?}",
            outcome.skipped_queue_errors,
        );
        assert!(
            outcome
                .skipped_queue_errors
                .iter()
                .any(|e| matches!(e, JiraConfigError::UnrecognisedMode { .. })),
            "got {:?}",
            outcome.skipped_queue_errors,
        );
    }

    #[test]
    fn errors_aggregate_across_queues() {
        let toml_str = r#"
[sources.jira]
url = "https://x.atlassian.net"
email = "a@b.c"
auth_token_env = "TOK"

[sources.jira.queues.foo]
filter_jql = "project = PROJ"
mode = "direct"

[sources.jira.queues.bar]
filter_jql = "project = OTHER"
mode = "weird"
"#;
        let parsed = parse(toml_str);
        let errs = build(&parsed).expect_err("both queues misconfigured");
        // Both errors surface in one validation pass.
        let kinds: Vec<&JiraConfigError> = errs.iter().collect();
        assert!(
            kinds
                .iter()
                .any(|e| matches!(e, JiraConfigError::DirectModeNotImplemented { queue } if queue == "foo")),
            "expected DirectModeNotImplemented for foo: {errs:?}",
        );
        assert!(
            kinds.iter().any(|e| matches!(
                e,
                JiraConfigError::UnrecognisedMode { queue, .. } if queue == "bar"
            )),
            "expected UnrecognisedMode for bar: {errs:?}",
        );
    }
}
