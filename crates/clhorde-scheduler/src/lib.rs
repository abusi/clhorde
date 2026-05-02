//! `clhorde-scheduler` — workflow runtime for spec-driven AI development.
//!
//! Phase 1 shipped the pure-algorithm pieces:
//! - [`openspec::tasks_parser`] turns an OpenSpec `tasks.md` into a structured
//!   [`TaskGraph`](openspec::tasks_parser::TaskGraph).
//! - [`openspec::annotations`] parses `<!-- clhorde: ... -->` directives
//!   adjacent to headers and task lines.
//! - [`openspec::dag`] builds an executable DAG from a `TaskGraph` and rejects
//!   cyclic configurations.
//!
//! Phase 2 adds the binary, the daemon client, the FS watcher, and the
//! workflow execution loop. Sub-phase 2.1 ships the [`cli`] argument types
//! and the [`daemon_client`] long-lived IPC connector. Real workflow logic
//! lands in 2.2+.

pub mod cli;
pub mod commands;
pub mod control;
pub mod daemon_client;
pub mod dispatch;
pub mod openspec;
pub mod orchestrator;
pub mod persistence;
pub mod templates;
pub mod watcher;
pub mod workflow;
