use std::path::{Path, PathBuf};
use std::process::Command;

/// Check if a directory is inside a git repo.
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "--git-dir"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Get the repo root directory (git rev-parse --show-toplevel).
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// Get the repo directory name for worktree naming.
pub fn repo_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string())
}

/// Check if a path is a registered git worktree of the given repo.
pub fn worktree_exists(repo_root: &Path, worktree_path: &Path) -> bool {
    if !worktree_path.is_dir() {
        return false;
    }
    let output = match Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let canonical = worktree_path.canonicalize().ok();
    for line in stdout.lines() {
        if let Some(listed) = line.strip_prefix("worktree ") {
            let listed_path = Path::new(listed);
            if listed_path == worktree_path {
                return true;
            }
            if let Some(ref canon) = canonical {
                if let Ok(listed_canon) = listed_path.canonicalize() {
                    if listed_canon == *canon {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Sanitize an arbitrary string into a safe worktree directory name suffix.
/// Keeps `[A-Za-z0-9._-]`, replaces everything else with `-`, collapses runs.
fn sanitize_worktree_suffix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "wt".to_string()
    } else {
        trimmed
    }
}

/// Create a detached worktree named after a prompt id:
/// `git worktree add --detach <repo>-wt-<prompt_id> HEAD`.
pub fn create_worktree(repo_root: &Path, prompt_id: usize) -> Result<PathBuf, String> {
    create_worktree_named(repo_root, &prompt_id.to_string())
}

/// Create a detached worktree with a caller-supplied suffix.
/// Directory layout: `<parent>/<repo>-wt-<sanitized_suffix>`.
/// If the worktree already exists (registered with git), it is reused.
pub fn create_worktree_named(repo_root: &Path, suffix: &str) -> Result<PathBuf, String> {
    let name = repo_name(repo_root);
    let safe_suffix = sanitize_worktree_suffix(suffix);
    let wt_dir = format!("{name}-wt-{safe_suffix}");
    let parent = repo_root
        .parent()
        .ok_or_else(|| "Cannot determine parent directory of repo root".to_string())?;
    let wt_path = parent.join(&wt_dir);

    if worktree_exists(repo_root, &wt_path) {
        return Ok(wt_path);
    }

    let output = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "worktree",
            "add",
            "--detach",
            &wt_path.to_string_lossy(),
            "HEAD",
        ])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run git worktree add: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {stderr}"));
    }

    Ok(wt_path)
}

/// Remove a worktree: git worktree remove <path> --force
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "worktree",
            "remove",
            &worktree_path.to_string_lossy(),
            "--force",
        ])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run git worktree remove: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree remove failed: {stderr}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn repo_name_extracts_dirname() {
        assert_eq!(repo_name(Path::new("/home/user/myrepo")), "myrepo");
        assert_eq!(repo_name(Path::new("/foo/bar")), "bar");
    }

    #[test]
    fn repo_name_fallback_for_root() {
        assert_eq!(repo_name(Path::new("/")), "repo");
    }

    #[test]
    fn is_git_repo_false_for_tmp() {
        assert!(!is_git_repo(Path::new("/tmp")));
    }

    #[test]
    fn repo_root_none_for_non_repo() {
        assert!(repo_root(Path::new("/tmp")).is_none());
    }

    /// Helper: create a throwaway git repo inside a temp dir and return
    /// (temp_dir_handle, repo_path).  The temp dir is cleaned up when the
    /// returned `TempDir` is dropped.
    fn make_temp_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let repo = tmp.path().join("testrepo");
        fs::create_dir(&repo).unwrap();
        Command::new("git")
            .args(["init", &repo.to_string_lossy()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git init");
        // Configure identity so commits work in CI (no global git config).
        for args in [
            &[
                "-C",
                &*repo.to_string_lossy(),
                "config",
                "user.email",
                "test@test",
            ][..],
            &[
                "-C",
                &*repo.to_string_lossy(),
                "config",
                "user.name",
                "test",
            ][..],
        ] {
            Command::new("git")
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git config");
        }
        // Need at least one commit for worktrees to work.
        Command::new("git")
            .args([
                "-C",
                &repo.to_string_lossy(),
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git commit");
        (tmp, repo)
    }

    #[test]
    fn create_worktree_then_reuse_on_second_call() {
        let (_tmp, repo) = make_temp_repo();
        let prompt_id = 42;

        // First call: creates the worktree.
        let wt = create_worktree(&repo, prompt_id).expect("first create");
        assert!(wt.is_dir(), "worktree directory should exist");

        // Second call with the same id: should succeed by reusing.
        let wt2 = create_worktree(&repo, prompt_id).expect("second create (reuse)");
        assert_eq!(wt, wt2);
    }

    #[test]
    fn worktree_exists_false_for_non_worktree() {
        let (_tmp, repo) = make_temp_repo();
        let bogus = repo.parent().unwrap().join("not-a-worktree");
        fs::create_dir(&bogus).unwrap();
        assert!(!worktree_exists(&repo, &bogus));
    }

    #[test]
    fn worktree_exists_true_after_creation() {
        let (_tmp, repo) = make_temp_repo();
        let wt = create_worktree(&repo, 99).expect("create");
        assert!(worktree_exists(&repo, &wt));
    }

    #[test]
    fn worktree_exists_false_for_missing_dir() {
        let (_tmp, repo) = make_temp_repo();
        let missing = repo.parent().unwrap().join("does-not-exist");
        assert!(!worktree_exists(&repo, &missing));
    }

    #[test]
    fn sanitize_worktree_suffix_passthrough() {
        assert_eq!(sanitize_worktree_suffix("abc-123"), "abc-123");
        assert_eq!(sanitize_worktree_suffix("Foo.Bar_42"), "Foo.Bar_42");
    }

    #[test]
    fn sanitize_worktree_suffix_replaces_unsafe() {
        assert_eq!(sanitize_worktree_suffix("a/b\\c"), "a-b-c");
        assert_eq!(sanitize_worktree_suffix("hello world!"), "hello-world");
    }

    #[test]
    fn sanitize_worktree_suffix_collapses_runs() {
        assert_eq!(sanitize_worktree_suffix("a   b"), "a-b");
        assert_eq!(sanitize_worktree_suffix("//a//"), "a");
    }

    #[test]
    fn sanitize_worktree_suffix_empty_falls_back() {
        assert_eq!(sanitize_worktree_suffix(""), "wt");
        assert_eq!(sanitize_worktree_suffix("///"), "wt");
    }

    #[test]
    fn create_worktree_named_uses_suffix() {
        let (_tmp, repo) = make_temp_repo();
        let wt = create_worktree_named(&repo, "my-shared-id").expect("create");
        assert!(wt.is_dir());
        let name = wt.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.ends_with("-wt-my-shared-id"),
            "unexpected dir name: {name}"
        );
    }

    #[test]
    fn create_worktree_named_idempotent() {
        let (_tmp, repo) = make_temp_repo();
        let wt = create_worktree_named(&repo, "shared").expect("first");
        let wt2 = create_worktree_named(&repo, "shared").expect("second reuses");
        assert_eq!(wt, wt2);
    }
}
