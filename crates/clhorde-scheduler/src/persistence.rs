//! Persist workflows to `~/.local/share/clhorde/workflows/<name>.json`.
//!
//! One file per workflow keyed by `name`. JSON, atomic writes via
//! `tempfile + rename`. Every new field on [`Workflow`] uses
//! `#[serde(default)]` so older files keep loading after a schema bump.
//!
//! The store is intentionally simple: `WorkflowStore::open` takes the
//! directory directly, which makes it trivial to redirect to a tempdir in
//! tests without monkey-patching XDG. Production callers use
//! [`default_dir`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::workflow::Workflow;

/// Default workflow directory: `~/.local/share/clhorde/workflows/`.
/// Returns `None` only if `dirs::data_dir()` itself fails (no `$HOME`).
pub fn default_dir() -> Option<PathBuf> {
    clhorde_core::config::data_dir().map(|d| d.join("workflows"))
}

/// File-backed store for workflows. Cheap to clone — it only holds a path.
#[derive(Debug, Clone)]
pub struct WorkflowStore {
    dir: PathBuf,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    InvalidName(String),
    Json(serde_json::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::InvalidName(n) => write!(f, "invalid workflow name {n:?}"),
            StoreError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            StoreError::Json(e) => Some(e),
            StoreError::InvalidName(_) => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

impl WorkflowStore {
    /// Open a store rooted at `dir`. The directory is created lazily on first
    /// write — `open` itself never touches the filesystem.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Open the default user store under `~/.local/share/clhorde/workflows/`.
    pub fn open_default() -> Result<Self, StoreError> {
        let dir = default_dir().ok_or_else(|| {
            StoreError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "no XDG data directory available",
            ))
        })?;
        Ok(Self::open(dir))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Persist a workflow. Atomically replaces any existing file.
    pub fn save(&self, wf: &Workflow) -> Result<(), StoreError> {
        let path = self.path_for(&wf.name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(wf)?;

        // Write to a sibling temp file then rename — guarantees readers never
        // observe a half-written file.
        let mut tmp = path.clone();
        tmp.set_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Load a workflow by name. Returns `Ok(None)` if the file does not exist.
    pub fn load(&self, name: &str) -> Result<Option<Workflow>, StoreError> {
        let path = self.path_for(name)?;
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e)),
        };
        let wf: Workflow = serde_json::from_slice(&bytes)?;
        Ok(Some(wf))
    }

    /// Remove a workflow file. Missing files are not an error — caller intent
    /// is met either way.
    pub fn delete(&self, name: &str) -> Result<(), StoreError> {
        let path = self.path_for(name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// List every persisted workflow, sorted by name.
    pub fn list(&self) -> Result<Vec<Workflow>, StoreError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(e)),
        };

        let mut out: Vec<Workflow> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping workflow file that could not be read"
                    );
                    continue;
                }
            };
            match serde_json::from_slice::<Workflow>(&bytes) {
                Ok(wf) => out.push(wf),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping malformed workflow file"
                    );
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn path_for(&self, name: &str) -> Result<PathBuf, StoreError> {
        if !is_valid_name(name) {
            return Err(StoreError::InvalidName(name.to_string()));
        }
        Ok(self.dir.join(format!("{name}.json")))
    }
}

/// Workflow names must match the OpenSpec change directory naming rule —
/// effectively "filesystem-safe ascii". We're stricter than OpenSpec on
/// purpose so a malicious or malformed name cannot escape the store
/// directory via traversal.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openspec::discovery::MarkerMetadata;
    use crate::workflow::{SourceKind, WorkflowStatus};
    use tempfile::TempDir;

    fn store_in(tmp: &TempDir) -> WorkflowStore {
        WorkflowStore::open(tmp.path().join("workflows"))
    }

    #[test]
    fn round_trip_through_disk() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let mut wf = Workflow::drafted("add-oauth");
        wf.queue(MarkerMetadata {
            priority: Some(7),
            ..MarkerMetadata::default()
        })
        .unwrap();
        wf.start_implementing().unwrap();
        wf.prompt_ids.push("uuid-1".into());

        store.save(&wf).unwrap();

        let loaded = store.load("add-oauth").unwrap().unwrap();
        assert_eq!(loaded, wf);
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        assert!(store.load("nope").unwrap().is_none());
    }

    #[test]
    fn delete_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        store.delete("missing").unwrap();
    }

    #[test]
    fn save_then_delete_then_list_is_empty() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        store.save(&Workflow::drafted("x")).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        store.delete("x").unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_sorted_skipping_unrelated_files() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        store.save(&Workflow::drafted("zeta")).unwrap();
        store.save(&Workflow::drafted("alpha")).unwrap();
        // Drop a stray non-JSON file in the directory; should be ignored.
        fs::write(store.dir().join("README.md"), b"hi").unwrap();

        let names: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn list_skips_malformed_json() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("garbage.json"), b"{not valid").unwrap();
        store.save(&Workflow::drafted("good")).unwrap();

        let names: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["good".to_string()]);
    }

    #[test]
    fn invalid_name_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        // Path traversal attempt.
        let mut wf = Workflow::drafted("../../etc/passwd");
        wf.status = WorkflowStatus::Drafted;
        let err = store.save(&wf).unwrap_err();
        match err {
            StoreError::InvalidName(n) => assert_eq!(n, "../../etc/passwd"),
            other => panic!("expected InvalidName, got {other}"),
        }
    }

    #[test]
    fn dotted_name_accepted_dotfile_rejected() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        store
            .save(&Workflow::drafted("add-oauth.v2"))
            .expect("dotted name should be ok");
        let err = store.save(&Workflow::drafted(".hidden")).unwrap_err();
        assert!(matches!(err, StoreError::InvalidName(_)));
    }

    #[test]
    fn back_compat_default_for_new_field() {
        // A file written by an older scheduler that didn't know about
        // `prompt_ids` / `metadata` should still load.
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        fs::create_dir_all(store.dir()).unwrap();
        let body = serde_json::json!({
            "name": "old-format",
            "status": { "kind": "drafted" }
        });
        fs::write(
            store.dir().join("old-format.json"),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();

        let loaded = store.load("old-format").unwrap().unwrap();
        assert_eq!(loaded.name, "old-format");
        assert_eq!(loaded.status, WorkflowStatus::Drafted);
        assert!(loaded.prompt_ids.is_empty());
        assert_eq!(loaded.metadata, MarkerMetadata::default());
        // Pre-jira-source workflow files have no `source` field; they
        // must default to OpenSpec on reload.
        assert_eq!(loaded.source, SourceKind::OpenSpec);
    }

    #[test]
    fn round_trip_preserves_exploring_and_jira_source() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let mut wf = Workflow::triggered("PROJ-1", SourceKind::Jira);
        wf.start_exploring().unwrap();
        store.save(&wf).unwrap();

        let loaded = store.load("PROJ-1").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Exploring);
        assert_eq!(loaded.source, SourceKind::Jira);
        assert_eq!(loaded, wf);
    }

    #[test]
    fn round_trip_preserves_triggered_state() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let wf = Workflow::triggered("PROJ-2", SourceKind::Jira);
        store.save(&wf).unwrap();

        let loaded = store.load("PROJ-2").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Triggered);
        assert_eq!(loaded.source, SourceKind::Jira);
        assert_eq!(loaded, wf);
    }

    #[test]
    fn legacy_file_with_explicit_openspec_source_loads() {
        // Forward compat: even if a future writer drops the source
        // field altogether, today's reader treats absence as OpenSpec.
        // Verify the explicit `"source": "open_spec"` form also loads.
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        fs::create_dir_all(store.dir()).unwrap();
        let body = serde_json::json!({
            "name": "explicit-openspec",
            "status": { "kind": "drafted" },
            "source": "open_spec"
        });
        fs::write(
            store.dir().join("explicit-openspec.json"),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();

        let loaded = store.load("explicit-openspec").unwrap().unwrap();
        assert_eq!(loaded.source, SourceKind::OpenSpec);
    }

    #[test]
    fn legacy_file_with_jira_source_loads() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        fs::create_dir_all(store.dir()).unwrap();
        let body = serde_json::json!({
            "name": "PROJ-3",
            "status": { "kind": "exploring" },
            "source": "jira"
        });
        fs::write(
            store.dir().join("PROJ-3.json"),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();

        let loaded = store.load("PROJ-3").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Exploring);
        assert_eq!(loaded.source, SourceKind::Jira);
    }

    #[test]
    fn save_overwrites_atomically() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);
        let mut wf = Workflow::drafted("x");
        store.save(&wf).unwrap();

        wf.queue(MarkerMetadata::default()).unwrap();
        store.save(&wf).unwrap();

        let loaded = store.load("x").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Queued);

        // No leftover .tmp file from the rename.
        let leftovers: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
