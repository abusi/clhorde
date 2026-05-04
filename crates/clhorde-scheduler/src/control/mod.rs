//! Scheduler control socket: live remote-control of a running
//! `clhorde-scheduler daemon`.
//!
//! The control socket lives at
//! [`clhorde_core::ipc::scheduler_socket_path`] (default
//! `~/.local/share/clhorde/scheduler.sock`) and uses the same
//! length-delimited JSON framing as the main daemon's socket. A client
//! sends one [`ControlRequest`], the scheduler replies with one
//! [`ControlResponse`], and the client may keep the connection open to
//! send more requests.
//!
//! This module exposes the wire protocol ([`protocol`]), the server
//! ([`server`]) hosted inside the scheduler binary, and a small client
//! helper ([`client`]) used by `clhorde-cli flow` and by future TUI
//! integration.

pub mod client;
pub mod protocol;
pub mod server;

pub use protocol::{
    ControlRequest, ControlResponse, DetailNode, SchedulerEvent, WorkflowDetail,
    WorkflowSummary,
};
