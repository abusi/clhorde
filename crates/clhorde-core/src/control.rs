//! Wire types for the scheduler control socket.
//!
//! Same length-delimited JSON framing as the daemon socket
//! ([`crate::ipc::encode_frame`]). These types live in `clhorde-core`
//! rather than `clhorde-scheduler` so the TUI and the web bridge can
//! pick them up without depending on the scheduler crate (and pulling
//! in tera, notify, clap, …) just to decode a `WorkflowSummary`.
//!
//! Server and client logic stays in `clhorde-scheduler::control` —
//! only the data shapes are shared here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Inbound request from a control-socket client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Liveness probe; the server always replies with [`ControlResponse::Pong`].
    Ping,

    /// Return one or all workflow summaries.
    ///
    /// - `name = None`: every known workflow, sorted by name.
    /// - `name = Some(n)`: just that one. The server emits
    ///   [`ControlResponse::Error`] if the workflow does not exist.
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// Cancel a workflow:
    /// 1. Remove `openspec/changes/<name>/.clhorde-ready` if present.
    /// 2. Transition the persisted workflow to `Cancelled` (or
    ///    `Drafted` if it was only queued, mirroring the FS-based
    ///    `cancel` command).
    /// 3. The orchestrator may instruct the daemon to terminate any
    ///    workers it had dispatched for this workflow.
    Cancel { name: String },

    /// Re-dispatch a single apply-phase section by its `tasks.md` id
    /// (e.g. `"3"` for a top-level section, `"3.2"` for a nested task).
    Retry { name: String, section: String },

    /// Queue a draft change by writing
    /// `openspec/changes/<name>/.clhorde-ready`. Errors if the change
    /// directory does not exist.
    Queue {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<i32>,
    },
}

/// Outbound response from the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Reply to [`ControlRequest::Ping`].
    Pong,

    /// Reply to [`ControlRequest::Status`]. May contain zero, one, or many
    /// summaries depending on the request shape.
    ///
    /// `root` carries the absolute path the scheduler is watching, so a
    /// client (TUI, web) can resolve `openspec/changes/<name>/...`
    /// without round-tripping. Optional for back-compat with older
    /// scheduler builds.
    Status {
        workflows: Vec<WorkflowSummary>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
    },

    /// Mutation succeeded. `message` is a single-line human summary that
    /// the CLI prints verbatim.
    Ok { message: String },

    /// Mutation or query failed. `message` is the error description.
    Error { message: String },
}

/// User-visible snapshot of one workflow. Carries the fields shown by
/// `clhorde-scheduler status <name>` so the same struct can power both
/// the CLI surface and the Phase 4 TUI tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSummary {
    pub name: String,
    /// Lower-case status label: `drafted`, `queued`, `implementing`,
    /// `verifying`, `archiving`, `archived`, `cancelled`, `failed`.
    pub status: String,
    /// Set when `status == "failed"`; carries the same string we'd show
    /// from `WorkflowStatus::Failed { reason }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Priority from the marker metadata (defaults to 0 when absent).
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub prompt_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_round_trip() {
        let json = serde_json::to_string(&ControlRequest::Ping).unwrap();
        assert_eq!(json, r#"{"type":"ping"}"#);
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ControlRequest::Ping);
    }

    #[test]
    fn status_no_name_round_trip() {
        let req = ControlRequest::Status { name: None };
        let json = serde_json::to_string(&req).unwrap();
        // `skip_serializing_if` strips the field when None.
        assert_eq!(json, r#"{"type":"status"}"#);
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn status_with_name_round_trip() {
        let req = ControlRequest::Status {
            name: Some("add-oauth".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""name":"add-oauth""#));
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn retry_round_trip() {
        let req = ControlRequest::Retry {
            name: "x".into(),
            section: "3.2".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn queue_round_trip_no_priority() {
        let req = ControlRequest::Queue {
            name: "add-oauth".into(),
            priority: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        // `skip_serializing_if` strips the field when None.
        assert_eq!(json, r#"{"type":"queue","name":"add-oauth"}"#);
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn queue_round_trip_with_priority() {
        let req = ControlRequest::Queue {
            name: "add-oauth".into(),
            priority: Some(5),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""priority":5"#));
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn status_response_root_round_trip() {
        let resp = ControlResponse::Status {
            workflows: vec![],
            root: Some("/tmp/repo".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""root":"/tmp/repo""#));
        let back: ControlResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn status_response_back_compat_no_root() {
        // Older scheduler builds don't include `root`. Decoding
        // must succeed and yield `None`.
        let json = r#"{"type":"status","workflows":[]}"#;
        let resp: ControlResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp,
            ControlResponse::Status {
                workflows: vec![],
                root: None,
            }
        );
    }

    #[test]
    fn response_ok_and_error_distinguish() {
        let ok = ControlResponse::Ok {
            message: "done".into(),
        };
        let err = ControlResponse::Error {
            message: "nope".into(),
        };
        assert_ne!(ok, err);
        let ok_json = serde_json::to_string(&ok).unwrap();
        let err_json = serde_json::to_string(&err).unwrap();
        assert!(ok_json.contains(r#""type":"ok""#));
        assert!(err_json.contains(r#""type":"error""#));
    }

    #[test]
    fn workflow_summary_round_trip_minimal() {
        let s = WorkflowSummary {
            name: "x".into(),
            status: "drafted".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec![],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: WorkflowSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn workflow_summary_back_compat_missing_optionals() {
        let json = r#"{"name":"x","status":"queued"}"#;
        let s: WorkflowSummary = serde_json::from_str(json).unwrap();
        assert_eq!(s.name, "x");
        assert_eq!(s.status, "queued");
        assert_eq!(s.priority, 0);
        assert!(s.prompt_ids.is_empty());
        assert!(s.queued_at.is_none());
    }

    #[test]
    fn unknown_request_type_is_an_error() {
        let json = r#"{"type":"shutdown"}"#;
        let res: Result<ControlRequest, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }
}
