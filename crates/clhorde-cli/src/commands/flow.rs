//! `clhorde-cli flow <subcommand>` — thin wrapper that forwards every
//! argument to `clhorde-scheduler <subcommand>`.
//!
//! The point is to give users a single CLI entrypoint (`clhorde-cli`)
//! while the scheduler binary stays usable on its own. We deliberately
//! do not re-derive the scheduler's clap surface here: forwarding the
//! raw `argv` keeps the two CLIs in lockstep without duplication and
//! avoids re-implementing every flag.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

const SCHEDULER_BINARY: &str = "clhorde-scheduler";

/// Execute `clhorde-scheduler <args>` and return its exit code (or `1`
/// if the binary cannot be spawned).
pub fn cmd_flow(args: &[String]) -> i32 {
    run(args, resolve_scheduler_binary())
}

/// Test hook that lets tests inject an arbitrary binary path. Production
/// callers go through [`cmd_flow`].
pub fn run<P: AsRef<std::path::Path>>(args: &[String], binary: P) -> i32 {
    let bin = binary.as_ref();
    let mut cmd = Command::new(bin);
    for a in args {
        cmd.arg::<&OsString>(&a.into());
    }
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!(
                "error: failed to execute {}: {e}\n\
                 hint: install clhorde-scheduler or put it on PATH alongside clhorde-cli",
                bin.display()
            );
            1
        }
    }
}

/// Resolve the scheduler binary. The sibling-of-current-exe path is
/// preferred so a `target/release/clhorde-cli` invokes the matching
/// `target/release/clhorde-scheduler` even when the release tree isn't
/// on `PATH`. Falls back to the bare name so a system-wide install
/// (`/usr/local/bin/clhorde-scheduler`) still works.
fn resolve_scheduler_binary() -> PathBuf {
    if let Ok(current) = env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join(SCHEDULER_BINARY);
            #[cfg(windows)]
            let sibling = sibling.with_extension("exe");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from(SCHEDULER_BINARY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Build a tiny shell script that records its argv and exits with the
    /// given code. Returns the script path.
    #[cfg(unix)]
    fn fake_binary(tmp: &TempDir, exit_code: i32, log: &std::path::Path) -> PathBuf {
        let path = tmp.path().join("fake-scheduler");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nexit {exit_code}\n",
            log.display()
        );
        fs::write(&path, body).unwrap();
        let mut perm = fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&path, perm).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn forwards_args_verbatim() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("argv.log");
        let bin = fake_binary(&tmp, 0, &log);

        let code = run(
            &[
                "status".to_string(),
                "add-oauth".to_string(),
                "--root".to_string(),
                "/tmp/x".to_string(),
            ],
            &bin,
        );
        assert_eq!(code, 0);

        let recorded = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(lines, vec!["status", "add-oauth", "--root", "/tmp/x"]);
    }

    #[cfg(unix)]
    #[test]
    fn propagates_non_zero_exit() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("argv.log");
        let bin = fake_binary(&tmp, 7, &log);
        let code = run(&["queue".to_string(), "x".to_string()], &bin);
        assert_eq!(code, 7);
    }

    #[test]
    fn missing_binary_returns_one_with_hint() {
        // Pick a path that definitely does not exist.
        let phantom = std::env::temp_dir().join("clhorde-flow-no-such-binary");
        let _ = fs::remove_file(&phantom);
        let code = run(&["status".to_string()], &phantom);
        assert_eq!(code, 1);
    }
}
