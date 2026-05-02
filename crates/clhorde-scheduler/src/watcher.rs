//! Filesystem watcher that turns raw notify events into the small set of
//! [`FsEvent`]s the orchestrator cares about.
//!
//! The translation logic is split into two pure functions —
//! [`classify_path`] and [`classify_event`] — so the orchestrator's tests can
//! synthesize events without touching `notify` at all. The runtime side
//! ([`spawn`]) is a thin shell that wires `notify_debouncer_full` to a
//! channel and forwards classified events.
//!
//! Why "does the file currently exist" rather than "what kind of event":
//! `notify` reports `Create` / `Modify` / `Remove` differently across
//! platforms (an editor's atomic save is a remove + rename on Linux, a single
//! modify on macOS, etc.). By re-checking the file on disk after each event,
//! we collapse all of those into the two states the scheduler actually cares
//! about: marker-present or marker-absent. Debouncing then absorbs the
//! resulting flurry of redundant events into a single `FsEvent`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::sync::mpsc;

/// Filename of the queue marker, mirrored from `discovery` so the watcher can
/// be used without pulling that module in.
pub const MARKER_FILENAME: &str = ".clhorde-ready";
/// Filename of the OpenSpec tasks file.
pub const TASKS_FILENAME: &str = "tasks.md";

/// Coarse description of what a path points at, relative to
/// `<root>/openspec/changes/`. Internal — exported only for tests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathTarget {
    Marker { change_name: String },
    TasksMd { change_name: String },
}

/// Event the orchestrator processes. Decoupled from `notify` so we can
/// drive the orchestrator with synthetic events in unit tests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FsEvent {
    MarkerCreated { name: String },
    MarkerRemoved { name: String },
    TasksModified { name: String },
}

/// Pure path classifier. Returns `Some` if `path` points at the marker or the
/// tasks file of a change directory directly under `<root>/openspec/changes/`,
/// and `None` for everything else (including paths nested deeper, dotfile
/// changes like `.archive`, or unrelated files).
pub fn classify_path(root: &Path, path: &Path) -> Option<PathTarget> {
    let changes_root = root.join("openspec").join("changes");
    let rel = path.strip_prefix(&changes_root).ok()?;

    let mut components = rel.iter();
    let change_name = components.next()?.to_str()?.to_string();
    if change_name.starts_with('.') {
        return None;
    }
    let file_name = components.next()?.to_str()?;
    if components.next().is_some() {
        // Deeper than `<change>/<file>` — not a target the watcher tracks.
        return None;
    }
    match file_name {
        MARKER_FILENAME => Some(PathTarget::Marker { change_name }),
        TASKS_FILENAME => Some(PathTarget::TasksMd { change_name }),
        _ => None,
    }
}

/// Translate a single notify path into an [`FsEvent`]. The decision between
/// `MarkerCreated` and `MarkerRemoved` is based on the file's *current*
/// presence on disk, which is robust to platform-dependent event semantics.
///
/// `tasks.md` events are only emitted when the file is currently present —
/// a deleted tasks file produces no event (the orchestrator can't re-parse
/// nothing, and `tasks.md` getting removed is not a state-machine trigger
/// in this phase).
pub fn classify_event(root: &Path, path: &Path) -> Option<FsEvent> {
    let target = classify_path(root, path)?;
    let changes_root = root.join("openspec").join("changes");
    match target {
        PathTarget::Marker { change_name } => {
            let p = changes_root.join(&change_name).join(MARKER_FILENAME);
            if p.exists() {
                Some(FsEvent::MarkerCreated { name: change_name })
            } else {
                Some(FsEvent::MarkerRemoved { name: change_name })
            }
        }
        PathTarget::TasksMd { change_name } => {
            let p = changes_root.join(&change_name).join(TASKS_FILENAME);
            if p.exists() {
                Some(FsEvent::TasksModified { name: change_name })
            } else {
                None
            }
        }
    }
}

/// Handle returned by [`spawn`]. Dropping it stops the watcher.
pub struct WatcherHandle {
    // The debouncer owns the underlying notify watcher; we just keep it alive.
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    _join: std::thread::JoinHandle<()>,
}

/// Default debounce window. Tuned to absorb editor save bursts (atomic
/// rename, swap-file fiddling) without making the user wait noticeably.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Spawn a real filesystem watcher on `<root>/openspec/changes/`. Each
/// classified [`FsEvent`] is forwarded to `tx`. The watcher stays alive
/// until the returned [`WatcherHandle`] is dropped.
///
/// Errors:
/// - `<root>/openspec/changes/` must exist (we don't create it — that would
///   surprise users running the scheduler against a non-OpenSpec repo).
/// - Any error from `notify` setting up the platform watcher.
pub fn spawn(
    root: PathBuf,
    tx: mpsc::UnboundedSender<FsEvent>,
) -> notify::Result<WatcherHandle> {
    spawn_with_debounce(root, tx, DEFAULT_DEBOUNCE)
}

/// Test-friendly variant of [`spawn`] that exposes the debounce duration —
/// integration tests use a tighter window so they don't have to sleep
/// half a second per event.
pub fn spawn_with_debounce(
    root: PathBuf,
    tx: mpsc::UnboundedSender<FsEvent>,
    debounce: Duration,
) -> notify::Result<WatcherHandle> {
    let (raw_tx, raw_rx) = std_mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(debounce, None, move |res| {
        let _ = raw_tx.send(res);
    })?;

    let watch_path = root.join("openspec").join("changes");
    debouncer.watch(&watch_path, RecursiveMode::Recursive)?;

    let root_for_thread = root.clone();
    let join = std::thread::spawn(move || {
        // Each batch from the debouncer can contain redundant events for the
        // same target (e.g. atomic save: remove + create on `.clhorde-ready`).
        // De-duplicate within a batch so the orchestrator doesn't see flapping
        // pairs of MarkerRemoved/MarkerCreated for one user action.
        for batch in raw_rx {
            match batch {
                Ok(events) => {
                    let mut emitted: HashSet<FsEvent> = HashSet::new();
                    for ev in events {
                        for path in &ev.paths {
                            if let Some(fs_ev) = classify_event(&root_for_thread, path) {
                                if emitted.insert(fs_ev.clone()) && tx.send(fs_ev).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(errors) => {
                    for e in errors {
                        tracing::warn!(error = %e, "filesystem watcher error");
                    }
                }
            }
        }
    });

    Ok(WatcherHandle {
        _debouncer: debouncer,
        _join: join,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn changes_dir(tmp: &TempDir) -> PathBuf {
        let p = tmp.path().join("openspec").join("changes");
        fs::create_dir_all(&p).unwrap();
        p
    }

    // ── classify_path ──

    #[test]
    fn classifies_marker_path() {
        let tmp = TempDir::new().unwrap();
        let target = classify_path(
            tmp.path(),
            &tmp.path()
                .join("openspec/changes/add-oauth/.clhorde-ready"),
        );
        assert_eq!(
            target,
            Some(PathTarget::Marker {
                change_name: "add-oauth".into()
            })
        );
    }

    #[test]
    fn classifies_tasks_md_path() {
        let tmp = TempDir::new().unwrap();
        let target = classify_path(
            tmp.path(),
            &tmp.path().join("openspec/changes/add-oauth/tasks.md"),
        );
        assert_eq!(
            target,
            Some(PathTarget::TasksMd {
                change_name: "add-oauth".into()
            })
        );
    }

    #[test]
    fn ignores_unrelated_files() {
        let tmp = TempDir::new().unwrap();
        let cases = [
            "openspec/changes/add-oauth/proposal.md",
            "openspec/changes/add-oauth/specs/api/spec.md",
            "openspec/specs/api/spec.md",
            "README.md",
        ];
        for rel in cases {
            assert_eq!(classify_path(tmp.path(), &tmp.path().join(rel)), None);
        }
    }

    #[test]
    fn ignores_dotfile_change_dirs() {
        let tmp = TempDir::new().unwrap();
        // OpenSpec uses `openspec/changes/.archive/...`; we don't watch it.
        let p = tmp
            .path()
            .join("openspec/changes/.archive/add-oauth/tasks.md");
        assert_eq!(classify_path(tmp.path(), &p), None);
    }

    #[test]
    fn ignores_paths_outside_root() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let p = other
            .path()
            .join("openspec/changes/add-oauth/tasks.md");
        assert_eq!(classify_path(tmp.path(), &p), None);
    }

    // ── classify_event ──

    #[test]
    fn marker_present_yields_created() {
        let tmp = TempDir::new().unwrap();
        let dir = changes_dir(&tmp).join("add-oauth");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".clhorde-ready"), "").unwrap();

        let ev = classify_event(tmp.path(), &dir.join(".clhorde-ready"));
        assert_eq!(
            ev,
            Some(FsEvent::MarkerCreated {
                name: "add-oauth".into()
            })
        );
    }

    #[test]
    fn marker_absent_yields_removed() {
        let tmp = TempDir::new().unwrap();
        let dir = changes_dir(&tmp).join("add-oauth");
        fs::create_dir_all(&dir).unwrap();
        // No marker on disk — classify should still emit Removed because the
        // path that fired the notify event identified the marker.

        let ev = classify_event(tmp.path(), &dir.join(".clhorde-ready"));
        assert_eq!(
            ev,
            Some(FsEvent::MarkerRemoved {
                name: "add-oauth".into()
            })
        );
    }

    #[test]
    fn tasks_md_present_yields_modified() {
        let tmp = TempDir::new().unwrap();
        let dir = changes_dir(&tmp).join("add-oauth");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tasks.md"), "## 1. A\n").unwrap();

        let ev = classify_event(tmp.path(), &dir.join("tasks.md"));
        assert_eq!(
            ev,
            Some(FsEvent::TasksModified {
                name: "add-oauth".into()
            })
        );
    }

    #[test]
    fn tasks_md_absent_yields_no_event() {
        let tmp = TempDir::new().unwrap();
        let dir = changes_dir(&tmp).join("add-oauth");
        fs::create_dir_all(&dir).unwrap();

        // tasks.md never created — the deletion event yields no FsEvent.
        let ev = classify_event(tmp.path(), &dir.join("tasks.md"));
        assert_eq!(ev, None);
    }

    // ── live watcher (smoke) ──
    //
    // Hits the real notify backend. Skipped (as a soft-pass) when the
    // platform's watcher cannot be constructed in CI containers.

    #[tokio::test]
    async fn live_watcher_reports_marker_creation() {
        let tmp = TempDir::new().unwrap();
        let _ = changes_dir(&tmp); // ensure path exists
        let dir = tmp.path().join("openspec/changes/add-oauth");
        fs::create_dir_all(&dir).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle =
            match spawn_with_debounce(tmp.path().to_path_buf(), tx, Duration::from_millis(100)) {
                Ok(h) => h,
                Err(_) => return, // platform watcher unavailable
            };

        // Give the watcher a moment to install.
        tokio::time::sleep(Duration::from_millis(50)).await;
        fs::write(dir.join(".clhorde-ready"), "").unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher should report creation")
            .expect("event channel still open");
        assert_eq!(
            received,
            FsEvent::MarkerCreated {
                name: "add-oauth".into()
            }
        );

        drop(handle);
    }
}
