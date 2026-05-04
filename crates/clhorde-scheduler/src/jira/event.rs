//! Jira-source events consumed by the orchestrator.
//!
//! These types are the Jira analogue of [`crate::watcher::FsEvent`]: they
//! carry just enough information for the orchestrator to advance a workflow
//! without coupling the FSM to the Jira REST shape. Section 4 of the
//! `add-jira-source` change wires up the poll loop that produces these
//! events; section 2 only needs the type so the unified dispatch path
//! ([`crate::source::SourceEvent`]) can compile and be exercised in tests.
//!
//! `JiraTicketPayload` is intentionally minimal. The first cut keeps
//! ticket data as one prompt blob (per design D5 in the change's
//! `design.md`); the structured fields below are the bare minimum needed
//! to render the explore-gate prompt template.

use serde::{Deserialize, Serialize};

/// Inbound Jira event consumed by the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JiraEvent {
    /// A ticket matched a configured JQL filter for the first time
    /// (or first time since scheduler startup). `key` is the issue key
    /// (e.g. `"PROJ-1234"`); the orchestrator uses it as the workflow
    /// name. `payload` carries the fields needed to render the explore
    /// prompt template.
    TicketAppeared { key: String, payload: JiraTicketPayload },

    /// A ticket previously matched the filter but no longer does. The
    /// orchestrator uses this to cancel `Triggered` / `Exploring`
    /// workflows whose source ticket was moved out from under them.
    TicketLeftFilter { key: String },
}

/// Minimal Jira ticket projection used to render the explore prompt
/// template. Section 3 will populate this from the REST API; section 2
/// only needs the shape so tests can synthesize one without going
/// through an HTTP client.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JiraTicketPayload {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub acceptance_criteria: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporter: Option<String>,
}
