//! OpenSpec-aware parsing helpers.
//!
//! See the parent crate docs for the bigger picture. This module owns
//! everything specific to the OpenSpec on-disk layout (`openspec/changes/<X>/`,
//! `tasks.md`, etc.) so that future workflow sources (Linear, GitHub Issues,
//! custom YAML) can sit beside it without bleeding the OpenSpec taxonomy
//! into the rest of the crate.

pub mod annotations;
pub mod dag;
pub mod discovery;
pub mod tasks_parser;
