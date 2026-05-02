//! Walk an OpenSpec repository and classify each `openspec/changes/<name>/`
//! directory as either a draft or a queued workflow.
//!
//! A change is *queued* iff it contains a `.clhorde-ready` marker file. The
//! marker is plain TOML — see [`MarkerMetadata`] for the recognized fields.
//! Anything we don't recognize is silently ignored so newer schedulers can
//! ship richer markers without breaking older ones.
//!
//! `scan` is total: malformed markers degrade to `Drafted` (with a warning
//! logged) rather than aborting the whole sweep, so a single broken file
//! cannot stall the watcher.
//!
//! Phase 2.2 only ships the read side. Phase 2.3 wires the watcher; Phase 2.4
//! adds writers (`queue` / `unqueue` subcommands).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename of the queue marker, relative to a change directory.
pub const MARKER_FILENAME: &str = ".clhorde-ready";

/// Optional metadata carried inside `.clhorde-ready`. Every field defaults
/// because the marker may be empty or partial.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MarkerMetadata {
    /// Higher runs first when workers free up.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Other change names that must be `Archived` before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Branch name for the workflow's shared worktree (default: detached HEAD).
    #[serde(default)]
    pub worktree_branch: Option<String>,
    /// Section ids the user marked safe to run in parallel.
    #[serde(default)]
    pub parallel_sections: Vec<String>,
    /// Override `auto_retry` for this workflow's apply phase.
    #[serde(default)]
    pub max_section_retries: Option<u32>,
}

impl MarkerMetadata {
    /// Parse a marker body (the contents of `.clhorde-ready`). Empty input
    /// returns the default; malformed TOML returns `Err`.
    pub fn parse(body: &str) -> Result<Self, toml::de::Error> {
        if body.trim().is_empty() {
            return Ok(Self::default());
        }
        toml::from_str(body)
    }
}

/// On-disk classification of a single change directory.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeStatus {
    /// No `.clhorde-ready` marker.
    Drafted,
    /// `.clhorde-ready` present and parsed.
    Queued(MarkerMetadata),
}

impl ChangeStatus {
    pub fn is_queued(&self) -> bool {
        matches!(self, ChangeStatus::Queued(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredChange {
    /// Directory name under `openspec/changes/`.
    pub name: String,
    /// Absolute (or root-relative) path to the change directory.
    pub path: PathBuf,
    pub status: ChangeStatus,
}

/// Walk `<root>/openspec/changes/*/` and return every change directory found.
/// Result is sorted by name for deterministic output.
///
/// Missing root or missing `openspec/changes/` returns an empty vector — this
/// is the expected steady state in a repo that has not adopted OpenSpec yet.
pub fn scan(root: impl AsRef<Path>) -> Vec<DiscoveredChange> {
    let changes_root = root.as_ref().join("openspec").join("changes");
    let entries = match fs::read_dir(&changes_root) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<DiscoveredChange> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        // Skip dotfiles like `.archive` — OpenSpec uses these for archived
        // changes, which the scheduler should not pick up as drafts.
        if name.starts_with('.') {
            continue;
        }

        let status = classify(&path);
        out.push(DiscoveredChange {
            name,
            path,
            status,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn classify(change_dir: &Path) -> ChangeStatus {
    let marker = change_dir.join(MARKER_FILENAME);
    let body = match fs::read_to_string(&marker) {
        Ok(b) => b,
        Err(_) => return ChangeStatus::Drafted,
    };
    match MarkerMetadata::parse(&body) {
        Ok(m) => ChangeStatus::Queued(m),
        Err(e) => {
            tracing::warn!(
                path = %marker.display(),
                error = %e,
                "malformed .clhorde-ready marker; treating change as drafted"
            );
            ChangeStatus::Drafted
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_change(root: &Path, name: &str) -> PathBuf {
        let p = root.join("openspec").join("changes").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn empty_repo_returns_no_changes() {
        let tmp = TempDir::new().unwrap();
        assert!(scan(tmp.path()).is_empty());
    }

    #[test]
    fn missing_openspec_dir_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        // Only create `openspec/specs/`, no `openspec/changes/`.
        fs::create_dir_all(tmp.path().join("openspec").join("specs")).unwrap();
        assert!(scan(tmp.path()).is_empty());
    }

    #[test]
    fn draft_change_has_no_marker() {
        let tmp = TempDir::new().unwrap();
        make_change(tmp.path(), "add-oauth");
        let out = scan(tmp.path());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "add-oauth");
        assert_eq!(out[0].status, ChangeStatus::Drafted);
    }

    #[test]
    fn marker_present_classifies_as_queued() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join(MARKER_FILENAME), "").unwrap();
        let out = scan(tmp.path());
        assert_eq!(out.len(), 1);
        match &out[0].status {
            ChangeStatus::Queued(m) => assert_eq!(m, &MarkerMetadata::default()),
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn marker_with_full_metadata_round_trips() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        let body = r#"
priority = 5
worktree_branch = "feature/oauth"
depends_on = ["add-base-auth", "add-secrets"]
parallel_sections = ["3", "4"]
max_section_retries = 2
"#;
        fs::write(p.join(MARKER_FILENAME), body).unwrap();
        let out = scan(tmp.path());
        match &out[0].status {
            ChangeStatus::Queued(m) => {
                assert_eq!(m.priority, Some(5));
                assert_eq!(m.worktree_branch.as_deref(), Some("feature/oauth"));
                assert_eq!(
                    m.depends_on,
                    vec!["add-base-auth".to_string(), "add-secrets".to_string()]
                );
                assert_eq!(
                    m.parallel_sections,
                    vec!["3".to_string(), "4".to_string()]
                );
                assert_eq!(m.max_section_retries, Some(2));
            }
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn marker_with_partial_metadata_uses_defaults() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join(MARKER_FILENAME), "priority = 7\n").unwrap();
        let out = scan(tmp.path());
        match &out[0].status {
            ChangeStatus::Queued(m) => {
                assert_eq!(m.priority, Some(7));
                assert!(m.depends_on.is_empty());
                assert!(m.worktree_branch.is_none());
            }
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        let body = r#"
priority = 3
future_field = "ignore me"
nested = { also = "ignored" }
"#;
        fs::write(p.join(MARKER_FILENAME), body).unwrap();
        let out = scan(tmp.path());
        match &out[0].status {
            ChangeStatus::Queued(m) => assert_eq!(m.priority, Some(3)),
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn malformed_marker_falls_back_to_drafted() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        // Not valid TOML.
        fs::write(p.join(MARKER_FILENAME), "this = is not [[valid").unwrap();
        let out = scan(tmp.path());
        assert_eq!(out[0].status, ChangeStatus::Drafted);
    }

    #[test]
    fn mixed_drafts_and_queued_are_sorted_by_name() {
        let tmp = TempDir::new().unwrap();
        make_change(tmp.path(), "zeta");
        let p = make_change(tmp.path(), "alpha");
        fs::write(p.join(MARKER_FILENAME), "").unwrap();
        make_change(tmp.path(), "mu");

        let out = scan(tmp.path());
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
        assert!(out[0].status.is_queued());
        assert!(!out[1].status.is_queued());
        assert!(!out[2].status.is_queued());
    }

    #[test]
    fn dotfiles_are_skipped() {
        let tmp = TempDir::new().unwrap();
        // OpenSpec stores archived changes under `openspec/changes/.archive/`.
        // The scheduler should ignore them so they don't show up as drafts.
        make_change(tmp.path(), ".archive");
        make_change(tmp.path(), "real-change");

        let names: Vec<String> = scan(tmp.path())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["real-change".to_string()]);
    }

    #[test]
    fn files_at_changes_root_are_ignored() {
        let tmp = TempDir::new().unwrap();
        // A stray file like `README.md` next to the change dirs must not crash.
        let changes = tmp.path().join("openspec").join("changes");
        fs::create_dir_all(&changes).unwrap();
        fs::write(changes.join("README.md"), "stray\n").unwrap();
        make_change(tmp.path(), "add-oauth");

        let names: Vec<String> = scan(tmp.path()).into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["add-oauth".to_string()]);
    }
}
