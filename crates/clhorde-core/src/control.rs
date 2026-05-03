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

    /// Fetch a [`WorkflowDetail`] for the given workflow. Returns
    /// [`ControlResponse::Error`] if the workflow does not exist.
    Detail { name: String },

    /// Switch the connection into push-based stream mode.
    ///
    /// The server replies with one [`ControlResponse::Event`] carrying a
    /// [`SchedulerEvent::Snapshot`] (initial state), then keeps the
    /// connection open and emits one [`ControlResponse::Event`] per
    /// orchestrator state change. The connection is one-way after the
    /// initial Subscribe — clients that need to issue further requests
    /// (Cancel, Retry, Detail, …) open a separate one-shot connection.
    Subscribe,

    /// Switch the connection into push-based stream mode, scoped to one
    /// workflow's [`WorkflowDetail`].
    ///
    /// On success the server replies with one [`ControlResponse::Event`]
    /// carrying a [`SchedulerEvent::DetailSnapshot`] (initial state),
    /// then keeps the connection open and emits one
    /// [`ControlResponse::Event`] with a [`SchedulerEvent::DetailUpdated`]
    /// every time that workflow's detail changes. Other workflows'
    /// events are filtered out server-side. If the workflow does not
    /// exist the server writes a [`ControlResponse::Error`] and closes
    /// the connection. Like [`ControlRequest::Subscribe`], the
    /// connection is one-way after the request.
    SubscribeDetail { name: String },

    /// Switch the connection into push-based stream mode covering
    /// **every** workflow's detail events.
    ///
    /// The server forwards every [`SchedulerEvent::DetailUpdated`]
    /// the orchestrator emits, with no per-workflow filter and no
    /// initial snapshot — clients that need an initial state should
    /// REST-fetch [`ControlRequest::Detail`] for whatever workflow
    /// they care about. Designed for the web bridge: one stream,
    /// many WS clients filtering on the SPA side. Like the other
    /// subscribe variants, the connection is one-way after the
    /// request.
    SubscribeAllDetails,
}

/// Outbound response from the server.
///
/// `Detail` carries a [`WorkflowDetail`] inline rather than boxed —
/// the size difference matters for stack-allocated enums but not for a
/// wire type that's deserialized in one shot per response. Boxing would
/// also complicate the `Serialize`/`Deserialize` JSON shape for clients.
#[allow(clippy::large_enum_variant)]
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

    /// Reply to [`ControlRequest::Detail`].
    Detail { detail: WorkflowDetail },

    /// Push frame on a long-lived [`ControlRequest::Subscribe`]
    /// connection. The first event after Subscribe is always a
    /// [`SchedulerEvent::Snapshot`]; subsequent events report
    /// individual workflow changes.
    Event { event: SchedulerEvent },
}

/// Push-mode event payload streamed over a Subscribe connection.
///
/// The server sends one [`SchedulerEvent::Snapshot`] right after the
/// client subscribes (so a fresh client doesn't need a separate Status
/// round-trip) and then streams [`SchedulerEvent::WorkflowUpdated`]
/// every time the orchestrator mutates a workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SchedulerEvent {
    /// Full state. Sent once per Subscribe (and re-sent if the
    /// broadcast channel lagged the subscriber and we need to
    /// re-establish a baseline).
    Snapshot {
        workflows: Vec<WorkflowSummary>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
    },

    /// One workflow's [`WorkflowSummary`] changed. Clients merge by
    /// `summary.name`.
    WorkflowUpdated { summary: WorkflowSummary },

    /// Initial frame on a [`ControlRequest::SubscribeDetail`]
    /// connection. Sent once right after the client subscribes and
    /// re-sent if the broadcast channel lagged the subscriber. Carries
    /// the full [`WorkflowDetail`] for the workflow named in the
    /// request — the server filters by name so subscribers only ever
    /// see their own workflow's detail events.
    DetailSnapshot { detail: WorkflowDetail },

    /// One workflow's [`WorkflowDetail`] changed. Streamed on a
    /// [`ControlRequest::SubscribeDetail`] connection (filtered to
    /// the subscribed name). Always carries the full detail rather
    /// than a diff — payloads are small and full-snapshot semantics
    /// keep merge logic on the client trivial.
    DetailUpdated { detail: WorkflowDetail },
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
    /// Names of inter-workflow dependencies that are blocking this
    /// workflow from leaving `Queued`. Populated by the scheduler only
    /// when `status == "queued"` and the dep evaluator returned
    /// `Pending`; empty in every other case (no deps, all deps
    /// archived, or the workflow is no longer queued). Defaults to
    /// empty for back-compat with older scheduler builds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

/// Per-workflow detail, returned by [`ControlRequest::Detail`].
///
/// Carries the same top-level fields as [`WorkflowSummary`] plus the
/// per-section / per-phase dispatch view the TUI uses to render a
/// workflow's DAG. Phase nodes (`apply`, `verify`, `archive`) appear in
/// dispatch order; the apply phase has one entry per DAG node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowDetail {
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Apply-phase nodes in DAG order. Empty until `tasks.md` has been
    /// parsed (i.e. for fresh workflows that haven't been picked up yet).
    #[serde(default)]
    pub apply: Vec<DetailNode>,
    /// Verify-phase node. `None` until the apply phase completes and the
    /// scheduler dispatches verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<DetailNode>,
    /// Archive-phase node. `None` until verify finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<DetailNode>,
    /// Same semantics as [`WorkflowSummary::blocked_by`]: names of
    /// inter-workflow deps holding this workflow in `Queued`. Empty
    /// otherwise. Defaults to empty for back-compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

/// One row in the workflow detail view. Models a single DAG node (for
/// `apply`) or the singleton verify/archive node, with its dispatch
/// state as observed by the orchestrator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetailNode {
    /// Stable id from the source. `tasks.md` section id for apply nodes
    /// (e.g. `"1"`, `"2.3"`); the literal `"verify"` / `"archive"` for
    /// the singleton phases.
    pub id: String,
    /// Human-readable title.
    pub label: String,
    /// Lifecycle label. One of `"pending"` (DAG node not dispatched yet),
    /// `"running"` (dispatched, no `WorkerFinished` yet), `"completed"`
    /// (worker exited 0 and tasks.md has the boxes ticked),
    /// `"failed"` (worker exited non-zero or boxes still unchecked).
    pub state: String,
    /// Daemon-assigned numeric prompt id, once `PromptAdded` arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<usize>,
    /// Daemon-assigned UUID, once `PromptAdded` arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_uuid: Option<String>,
    /// Worker exit code from `WorkerFinished`, if the worker finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Apply-only: predecessor node ids in the DAG, for rendering the
    /// indent/tree structure. Empty for verify/archive.
    #[serde(default)]
    pub depends_on: Vec<String>,
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
    fn detail_request_round_trip() {
        let req = ControlRequest::Detail { name: "x".into() };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"detail","name":"x"}"#);
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn detail_response_round_trip() {
        let detail = WorkflowDetail {
            name: "x".into(),
            status: "implementing".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            apply: vec![DetailNode {
                id: "1".into(),
                label: "Theme".into(),
                state: "completed".into(),
                prompt_id: Some(7),
                prompt_uuid: Some("u-7".into()),
                exit_code: Some(0),
                depends_on: vec![],
            }],
            verify: None,
            archive: None,
            blocked_by: vec![],
        };
        let resp = ControlResponse::Detail {
            detail: detail.clone(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"detail""#));
        let back: ControlResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn detail_node_back_compat_minimal() {
        // Future-compatible: a minimal node (no optionals, no deps)
        // must decode cleanly. This protects clients against scheduler
        // builds that omit fields we'd later add as optional.
        let json = r#"{"id":"1","label":"X","state":"pending"}"#;
        let n: DetailNode = serde_json::from_str(json).unwrap();
        assert_eq!(n.id, "1");
        assert!(n.prompt_id.is_none());
        assert!(n.depends_on.is_empty());
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
            blocked_by: vec![],
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
        assert!(s.blocked_by.is_empty());
        assert!(s.prompt_ids.is_empty());
        assert!(s.queued_at.is_none());
    }

    #[test]
    fn unknown_request_type_is_an_error() {
        let json = r#"{"type":"shutdown"}"#;
        let res: Result<ControlRequest, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn subscribe_request_round_trip() {
        let json = serde_json::to_string(&ControlRequest::Subscribe).unwrap();
        assert_eq!(json, r#"{"type":"subscribe"}"#);
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ControlRequest::Subscribe);
    }

    #[test]
    fn event_snapshot_round_trip() {
        let event = SchedulerEvent::Snapshot {
            workflows: vec![WorkflowSummary {
                name: "x".into(),
                status: "queued".into(),
                failure_reason: None,
                priority: 0,
                queued_at: None,
                started_at: None,
                finished_at: None,
                prompt_ids: vec![],
                blocked_by: vec![],
            }],
            root: Some("/tmp/repo".into()),
        };
        let resp = ControlResponse::Event {
            event: event.clone(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"event""#));
        assert!(json.contains(r#""type":"snapshot""#));
        let back: ControlResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn event_snapshot_back_compat_no_root() {
        // A scheduler that doesn't set root must still decode cleanly:
        // root is optional with `skip_serializing_if`, just like on
        // ControlResponse::Status.
        let json = r#"{"type":"event","event":{"type":"snapshot","workflows":[]}}"#;
        let resp: ControlResponse = serde_json::from_str(json).unwrap();
        match resp {
            ControlResponse::Event {
                event: SchedulerEvent::Snapshot { workflows, root },
            } => {
                assert!(workflows.is_empty());
                assert!(root.is_none());
            }
            other => panic!("expected Snapshot event, got {other:?}"),
        }
    }

    #[test]
    fn event_workflow_updated_round_trip() {
        let summary = WorkflowSummary {
            name: "y".into(),
            status: "implementing".into(),
            failure_reason: None,
            priority: 3,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec!["uuid-1".into()],
            blocked_by: vec![],
        };
        let resp = ControlResponse::Event {
            event: SchedulerEvent::WorkflowUpdated {
                summary: summary.clone(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"workflow_updated""#));
        let back: ControlResponse = serde_json::from_str(&json).unwrap();
        match back {
            ControlResponse::Event {
                event: SchedulerEvent::WorkflowUpdated { summary: got },
            } => assert_eq!(got, summary),
            other => panic!("expected WorkflowUpdated, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_kind_is_an_error() {
        // SchedulerEvent is a closed enum on the wire; an unknown event
        // type should fail to decode rather than silently dropping the
        // frame on a forward-compat client.
        let json =
            r#"{"type":"event","event":{"type":"bogus","name":"x"}}"#;
        let res: Result<ControlResponse, _> = serde_json::from_str(json);
        assert!(res.is_err());
    }

    #[test]
    fn subscribe_detail_request_round_trip() {
        let req = ControlRequest::SubscribeDetail {
            name: "add-oauth".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"subscribe_detail""#));
        assert!(json.contains(r#""name":"add-oauth""#));
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn subscribe_all_details_request_round_trip() {
        let json = serde_json::to_string(&ControlRequest::SubscribeAllDetails).unwrap();
        assert_eq!(json, r#"{"type":"subscribe_all_details"}"#);
        let back: ControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ControlRequest::SubscribeAllDetails);
    }

    fn sample_detail() -> WorkflowDetail {
        WorkflowDetail {
            name: "z".into(),
            status: "implementing".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            apply: vec![DetailNode {
                id: "1".into(),
                label: "Setup".into(),
                state: "running".into(),
                prompt_id: Some(7),
                prompt_uuid: None,
                exit_code: None,
                depends_on: vec![],
            }],
            verify: None,
            archive: None,
            blocked_by: vec![],
        }
    }

    #[test]
    fn event_detail_snapshot_round_trip() {
        let detail = sample_detail();
        let resp = ControlResponse::Event {
            event: SchedulerEvent::DetailSnapshot {
                detail: detail.clone(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"event""#));
        assert!(json.contains(r#""type":"detail_snapshot""#));
        let back: ControlResponse = serde_json::from_str(&json).unwrap();
        match back {
            ControlResponse::Event {
                event: SchedulerEvent::DetailSnapshot { detail: got },
            } => assert_eq!(got, detail),
            other => panic!("expected DetailSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn event_detail_updated_round_trip() {
        let detail = sample_detail();
        let resp = ControlResponse::Event {
            event: SchedulerEvent::DetailUpdated {
                detail: detail.clone(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"detail_updated""#));
        let back: ControlResponse = serde_json::from_str(&json).unwrap();
        match back {
            ControlResponse::Event {
                event: SchedulerEvent::DetailUpdated { detail: got },
            } => assert_eq!(got, detail),
            other => panic!("expected DetailUpdated, got {other:?}"),
        }
    }

    // ── blocked_by (Phase 5.4.2) ──

    #[test]
    fn workflow_summary_with_blocked_by_round_trips() {
        let s = WorkflowSummary {
            name: "x".into(),
            status: "queued".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec![],
            blocked_by: vec!["a".into(), "b".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            json.contains(r#""blocked_by":["a","b"]"#),
            "blocked_by missing in serialized form: {json}"
        );
        let back: WorkflowSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn workflow_summary_omits_blocked_by_when_empty() {
        // skip_serializing_if = "Vec::is_empty" keeps the wire payload
        // identical to what older schedulers (without the field) emit,
        // so a TUI/web upgraded ahead of the scheduler stays compatible.
        let s = WorkflowSummary {
            name: "x".into(),
            status: "drafted".into(),
            failure_reason: None,
            priority: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            prompt_ids: vec![],
            blocked_by: vec![],
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("blocked_by"),
            "empty blocked_by should be skipped: {json}"
        );
    }

    #[test]
    fn workflow_detail_blocked_by_round_trips_and_back_compat() {
        // New payload encodes + decodes the field.
        let mut d = sample_detail();
        d.status = "queued".into();
        d.blocked_by = vec!["upstream".into()];
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(r#""blocked_by":["upstream"]"#));
        let back: WorkflowDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);

        // Old payload (no blocked_by field) decodes cleanly with default.
        let legacy = r#"{"name":"x","status":"queued","apply":[]}"#;
        let parsed: WorkflowDetail = serde_json::from_str(legacy).unwrap();
        assert!(parsed.blocked_by.is_empty());
    }
}
