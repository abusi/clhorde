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
        .args(["-C", &path.to_string_lossy(), "rev-parse", "--show-toplevel"])
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

/// Create a detached worktree: git worktree add --detach <path> HEAD
/// Returns the worktree path on success.
pub fn create_worktree(repo_root: &Path, prompt_id: usize) -> Result<PathBuf, String> {
    let name = repo_name(repo_root);
    let wt_dir = format!("{name}-wt-{prompt_id}");
    let parent = repo_root
        .parent()
        .ok_or_else(|| "Cannot determine parent directory of repo root".to_string())?;
    let wt_path = parent.join(&wt_dir);

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
}
