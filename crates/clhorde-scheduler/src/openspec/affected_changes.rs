//! Snapshot + diff of `<root>/openspec/changes/` for the
//! `openspec.affected_changes` annotation.
//!
//! Workflow:
//! 1. When a worker starts, capture a [`ChangesSnapshot`] of the prompt's
//!    effective working directory.
//! 2. When the worker finishes, capture a second snapshot.
//! 3. [`diff`] between the two yields a sorted, de-duplicated list of
//!    OpenSpec change names whose contents differ — this is exactly what
//!    the scheduler writes back to the daemon as the
//!    `openspec.affected_changes` annotation (defined as a generic
//!    primitive in Phase 0.3).
//!
//! Snapshots compare *content* — the per-file fingerprint mixes size with a
//! stdlib hash of the bytes, so atomic-rename saves with the same logical
//! content do not produce false-positive diffs. We deliberately do not
//! inspect mtime: editor-driven save patterns vary across platforms enough
//! that mtime-only fingerprints are unreliable.
//!
//! `walkdir` is intentionally not pulled in — `openspec/changes/<X>/` trees
//! are small (a handful of markdown files plus a `specs/` subtree) and
//! `std::fs::read_dir` recursion is fine.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Per-file fingerprint inside one change directory. The pair is
/// `(byte_size, content_hash)` — both must match for the file to be
/// considered unchanged.
type FileFingerprint = (u64, u64);

/// Snapshot of every `openspec/changes/<name>/` directory at one point in
/// time. Comparable so callers can detect what the next worker run
/// touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangesSnapshot {
    pub entries: BTreeMap<String, ChangeFingerprint>,
}

/// Recursive content fingerprint of a single change directory. Keyed by
/// relative path so the snapshot is independent of the absolute repo root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeFingerprint {
    pub files: BTreeMap<String, FileFingerprint>,
}

/// Walk `<root>/openspec/changes/*/` and produce a content-addressed
/// snapshot. Missing root or missing `openspec/changes/` returns an empty
/// snapshot — the same shape a fresh repo would yield.
pub fn snapshot(root: &Path) -> ChangesSnapshot {
    let changes_root = root.join("openspec").join("changes");
    let entries = match fs::read_dir(&changes_root) {
        Ok(rd) => rd,
        Err(_) => return ChangesSnapshot::default(),
    };

    let mut out = ChangesSnapshot::default();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // Skip dotfile change dirs (`.archive` and friends) so OpenSpec's
        // own bookkeeping never shows up as "affected".
        if name.starts_with('.') {
            continue;
        }
        let fp = fingerprint_dir(&entry.path());
        out.entries.insert(name, fp);
    }
    out
}

/// Return change names that differ between `before` and `after`, sorted.
/// A change is "affected" if it was added, removed, or any of its files
/// changed (size or content hash).
pub fn diff(before: &ChangesSnapshot, after: &ChangesSnapshot) -> Vec<String> {
    let mut affected: Vec<String> = Vec::new();
    let mut keys: Vec<&str> = before
        .entries
        .keys()
        .chain(after.entries.keys())
        .map(|s| s.as_str())
        .collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        match (before.entries.get(k), after.entries.get(k)) {
            (Some(a), Some(b)) if a == b => {}
            _ => affected.push(k.to_string()),
        }
    }
    affected
}

fn fingerprint_dir(path: &Path) -> ChangeFingerprint {
    let mut files: BTreeMap<String, FileFingerprint> = BTreeMap::new();
    walk_files(path, path, &mut files);
    ChangeFingerprint { files }
}

fn walk_files(
    base: &Path,
    cur: &Path,
    out: &mut BTreeMap<String, FileFingerprint>,
) {
    let entries = match fs::read_dir(cur) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk_files(base, &path, out);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(base) else {
            continue;
        };
        let Some(rel_str) = rel.to_str() else {
            // Non-UTF-8 paths: skip rather than crash. The scheduler is
            // building an annotation, not reading the file content for
            // logic, so missing entries here are tolerable.
            continue;
        };
        let Some(fp) = read_file_fingerprint(&path) else {
            continue;
        };
        out.insert(rel_str.to_string(), fp);
    }
}

fn read_file_fingerprint(path: &Path) -> Option<FileFingerprint> {
    let bytes = fs::read(path).ok()?;
    let size = bytes.len() as u64;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some((size, hasher.finish()))
}

/// Convenience: snapshot a path that may be `None` (e.g. a prompt with no
/// known cwd). Returns an empty snapshot in the `None` case so callers can
/// pair it with a later snapshot without branching.
pub fn snapshot_opt(path: Option<&PathBuf>) -> ChangesSnapshot {
    match path {
        Some(p) => snapshot(p),
        None => ChangesSnapshot::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_change(root: &Path, name: &str) -> PathBuf {
        let p = root.join("openspec").join("changes").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn empty_repo_yields_empty_snapshot() {
        let tmp = TempDir::new().unwrap();
        let snap = snapshot(tmp.path());
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn missing_changes_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("openspec/specs")).unwrap();
        assert!(snapshot(tmp.path()).entries.is_empty());
    }

    #[test]
    fn snapshot_records_each_change_with_files() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join("proposal.md"), "why").unwrap();
        fs::write(p.join("design.md"), "how").unwrap();

        let snap = snapshot(tmp.path());
        assert_eq!(snap.entries.len(), 1);
        let fp = snap.entries.get("add-oauth").unwrap();
        let names: Vec<&str> = fp.files.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["design.md", "proposal.md"]);
    }

    #[test]
    fn snapshot_includes_nested_files() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::create_dir_all(p.join("specs/api")).unwrap();
        fs::write(p.join("specs/api/spec.md"), "delta").unwrap();

        let snap = snapshot(tmp.path());
        let fp = snap.entries.get("add-oauth").unwrap();
        assert!(fp.files.contains_key("specs/api/spec.md"));
    }

    #[test]
    fn dotfile_change_dirs_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), ".archive");
        fs::write(p.join("ignored.md"), "x").unwrap();
        make_change(tmp.path(), "real-change");

        let snap = snapshot(tmp.path());
        let names: Vec<&str> =
            snap.entries.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["real-change"]);
    }

    #[test]
    fn diff_detects_added_change() {
        let tmp = TempDir::new().unwrap();
        let before = snapshot(tmp.path());
        make_change(tmp.path(), "add-oauth");
        let after = snapshot(tmp.path());
        assert_eq!(diff(&before, &after), vec!["add-oauth".to_string()]);
    }

    #[test]
    fn diff_detects_removed_change() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        let before = snapshot(tmp.path());
        fs::remove_dir_all(&p).unwrap();
        let after = snapshot(tmp.path());
        assert_eq!(diff(&before, &after), vec!["add-oauth".to_string()]);
    }

    #[test]
    fn diff_detects_modified_file_content() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join("proposal.md"), "version 1").unwrap();
        let before = snapshot(tmp.path());
        fs::write(p.join("proposal.md"), "version 2 (different bytes)").unwrap();
        let after = snapshot(tmp.path());
        assert_eq!(diff(&before, &after), vec!["add-oauth".to_string()]);
    }

    #[test]
    fn diff_ignores_unchanged_directories() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join("proposal.md"), "stable").unwrap();
        let before = snapshot(tmp.path());
        let after = snapshot(tmp.path());
        assert!(diff(&before, &after).is_empty());
    }

    #[test]
    fn diff_returns_sorted_unique_names() {
        let tmp = TempDir::new().unwrap();
        let before = snapshot(tmp.path());
        make_change(tmp.path(), "zeta");
        make_change(tmp.path(), "alpha");
        make_change(tmp.path(), "mu");
        let after = snapshot(tmp.path());
        assert_eq!(
            diff(&before, &after),
            vec![
                "alpha".to_string(),
                "mu".to_string(),
                "zeta".to_string()
            ]
        );
    }

    #[test]
    fn diff_detects_new_file_in_existing_change() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join("proposal.md"), "x").unwrap();
        let before = snapshot(tmp.path());
        fs::write(p.join("design.md"), "y").unwrap();
        let after = snapshot(tmp.path());
        assert_eq!(diff(&before, &after), vec!["add-oauth".to_string()]);
    }

    #[test]
    fn diff_picks_up_deleted_file_in_existing_change() {
        let tmp = TempDir::new().unwrap();
        let p = make_change(tmp.path(), "add-oauth");
        fs::write(p.join("proposal.md"), "x").unwrap();
        fs::write(p.join("design.md"), "y").unwrap();
        let before = snapshot(tmp.path());
        fs::remove_file(p.join("design.md")).unwrap();
        let after = snapshot(tmp.path());
        assert_eq!(diff(&before, &after), vec!["add-oauth".to_string()]);
    }

    #[test]
    fn snapshot_opt_handles_missing_path() {
        let snap = snapshot_opt(None);
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn changes_to_files_outside_changes_dir_are_invisible() {
        // The annotation describes "what change directories were touched".
        // Edits to specs/, README.md, or anything else must not surface.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("openspec").join("specs")).unwrap();
        fs::write(
            tmp.path().join("openspec").join("specs").join("api.md"),
            "v1",
        )
        .unwrap();
        let before = snapshot(tmp.path());
        fs::write(
            tmp.path().join("openspec").join("specs").join("api.md"),
            "v2",
        )
        .unwrap();
        let after = snapshot(tmp.path());
        assert!(diff(&before, &after).is_empty());
    }
}
