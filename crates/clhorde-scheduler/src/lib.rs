//! `clhorde-scheduler` — workflow runtime for spec-driven AI development.
//!
//! Phase 1 ships the pure-algorithm pieces:
//! - [`openspec::tasks_parser`] turns an OpenSpec `tasks.md` into a structured
//!   [`TaskGraph`](openspec::tasks_parser::TaskGraph).
//! - [`openspec::annotations`] parses `<!-- clhorde: ... -->` directives
//!   adjacent to headers and task lines.
//! - [`openspec::dag`] builds an executable DAG from a `TaskGraph` and rejects
//!   cyclic configurations.
//!
//! Phase 2 adds the binary, the daemon client, the FS watcher, and the
//! workflow execution loop. Until then this crate is library-only and has no
//! runtime side effects.

pub mod openspec;
