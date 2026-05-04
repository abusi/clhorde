//! Jira workflow source.
//!
//! Section 2 of the `add-jira-source` change shipped the event types and
//! the orchestrator-side plumbing. Section 3 added the thin async REST
//! client. Section 4 (this module's `source` submodule) wires the
//! polling loop that turns JQL filters into `JiraEvent`s for the
//! orchestrator. Section 6 ([`writeback`]) closes the loop with
//! fire-and-forget Jira write-back on workflow lifecycle events.
//! Section 8 ([`config`]) validates the parsed `[sources.jira]` block
//! into the runtime types the daemon assembles at startup.
//!
//! ## Layout
//! - [`event`] — types the orchestrator consumes (`JiraEvent`,
//!   `JiraTicketPayload`).
//! - [`client`] — async [`client::JiraClient`] for search, comment,
//!   transition, label add/remove. Retry policy lives here.
//! - [`auth`] — credential wrapper that keeps the token off `Debug`
//!   output and reads it from a configured env var.
//! - [`error`] — [`error::JiraError`] returned by every fallible
//!   operation in the module; used by the source's `last_jira_error`
//!   surface.
//! - [`source`] — per-queue poll loop with in-memory diffing,
//!   on-disk last-seen snapshot, and the `mpsc::Sender`-fed runtime
//!   spawned by the daemon binary.
//! - [`writeback`] — fire-and-forget driver for Jira comments, label
//!   removal, and optional status transitions on workflow lifecycle
//!   events.
//! - [`config`] — validation rules that turn the parsed
//!   `[sources.jira]` TOML schema into [`JiraConfig`], rejecting
//!   `mode = "direct"` queues with a clear pointer to the follow-up
//!   change.

pub mod auth;
pub mod client;
pub mod config;
pub mod error;
pub mod event;
pub mod source;
pub mod writeback;

pub use auth::JiraAuth;
pub use client::{BackoffPolicy, JiraClient};
pub use config::{
    build as build_config, build_partial as build_config_partial, JiraConfig, JiraConfigError,
    JiraQueue, ValidationOutcome, MODE_DIRECT, MODE_OPENSPEC,
};
pub use error::JiraError;
pub use event::{JiraEvent, JiraTicketPayload};
pub use source::{
    spawn as spawn_source, ConcurrencyGate, IssueSearch, JiraSource, JiraSourceConfig,
    JiraSourceHandle, JiraSourceStore, PollOutcome, QueueConfig,
    DEFAULT_MAX_CONCURRENT_EXPLORE, DEFAULT_MAX_RESULTS, DEFAULT_POLL_INTERVAL,
    MIN_POLL_INTERVAL,
};
pub use writeback::{
    JiraWriteback, JiraWritebackConfig, JiraWriter, LifecyclePhase, COMMENT_ARCHIVED,
    COMMENT_CANCELLED, COMMENT_EXPLORING, DEFAULT_TRIGGER_LABEL,
};
