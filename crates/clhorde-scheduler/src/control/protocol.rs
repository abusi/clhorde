//! Re-export of the control-socket wire types from `clhorde-core`.
//!
//! The shapes themselves moved to `clhorde_core::control` so the TUI,
//! the web bridge, and the CLI can import them cheaply (no transitive
//! tera/notify pull-in). Server and client behaviour stays in this
//! crate; only the data lives elsewhere.

pub use clhorde_core::control::{ControlRequest, ControlResponse, WorkflowSummary};
