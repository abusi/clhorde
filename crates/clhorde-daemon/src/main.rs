mod ipc_server;
mod orchestrator;
mod pty_worker;
mod session;
mod worker;

use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use clhorde_core::ipc::{daemon_pid_path, daemon_socket_path};
use clhorde_core::protocol::ClientRequest;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    eprintln!("clhorded v{VERSION} — clhorde daemon");
    eprintln!();
    eprintln!("Usage: clhorded [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -v       Enable info-level logging");
    eprintln!("  -vv      Enable debug-level logging");
    eprintln!("  --help   Print this help message");
}

/// Parse CLI args and return the log verbosity level.
/// Returns None if the program should exit (e.g. --help).
fn parse_args() -> Option<tracing::Level> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    parse_args_from(&args)
}

/// Inner parsing logic, testable without modifying std::env::args.
fn parse_args_from(args: &[String]) -> Option<tracing::Level> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return None;
    }

    // -vv as a single arg → debug
    if args.iter().any(|a| a == "-vv") {
        return Some(tracing::Level::DEBUG);
    }

    let v_count = args.iter().filter(|a| *a == "-v").count();

    let level = match v_count {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    };

    Some(level)
}

fn init_tracing(level: tracing::Level) {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level.to_string()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Check if a process with the given PID is alive.
/// Returns true if the process exists (even if owned by another user — EPERM).
fn is_process_alive(pid: i32) -> bool {
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // kill() returned -1; check errno
    let err = std::io::Error::last_os_error();
    // EPERM means the process exists but we lack permission to signal it
    err.raw_os_error() == Some(libc::EPERM)
}

/// Acquire an advisory file lock on the PID file, atomically preventing
/// two daemon instances from running concurrently.
///
/// Returns the open `File` handle — the lock is held as long as this handle
/// lives. On process exit (even crash/SIGKILL), the OS releases the lock.
fn acquire_pid_lock(pid_path: &Path) -> Result<File, String> {
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create dir: {e}"))?;
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(pid_path)
        .map_err(|e| format!("Failed to open PID file: {e}"))?;

    // Try to acquire an exclusive, non-blocking lock
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            // Lock is held by another process — read PID for the error message
            let mut content = String::new();
            let _ = file.read_to_string(&mut content);
            let pid_msg = content
                .trim()
                .parse::<i32>()
                .map(|pid| format!(" (PID {pid})"))
                .unwrap_or_default();
            return Err(format!("Daemon already running{pid_msg}"));
        }
        return Err(format!("Failed to lock PID file: {err}"));
    }

    // Lock acquired — check for stale PID and clean up stale socket
    let mut content = String::new();
    let _ = file.read_to_string(&mut content);
    if let Ok(old_pid) = content.trim().parse::<i32>() {
        if !is_process_alive(old_pid) {
            // Stale PID — clean up the socket too
            let _ = fs::remove_file(daemon_socket_path());
        }
    }

    // Write our PID
    file.set_len(0)
        .map_err(|e| format!("Failed to truncate PID file: {e}"))?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("Failed to seek PID file: {e}"))?;
    write!(file, "{}", process::id()).map_err(|e| format!("Failed to write PID: {e}"))?;
    file.flush()
        .map_err(|e| format!("Failed to flush PID file: {e}"))?;

    Ok(file)
}

fn cleanup_socket(socket_path: &PathBuf) {
    let _ = fs::remove_file(socket_path);
}

#[tokio::main]
async fn main() {
    // Parse CLI args before anything else (--help exits early)
    let log_level = match parse_args() {
        Some(level) => level,
        None => process::exit(0),
    };
    init_tracing(log_level);

    let pid_path = daemon_pid_path();
    let socket_path = daemon_socket_path();

    // 1. Acquire PID file lock (held for daemon lifetime; auto-released on exit)
    let _pid_lock = match acquire_pid_lock(&pid_path) {
        Ok(f) => f,
        Err(e) => {
            error!("{e}");
            process::exit(1);
        }
    };

    // Clean up any stale socket
    let _ = fs::remove_file(&socket_path);

    // 2. Create orchestrator
    let mut orch = orchestrator::Orchestrator::new();

    // 3. Channels for IPC server <-> main loop
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ipc_server::ServerCommand>();
    let (session_register_tx, mut session_register_rx) =
        mpsc::unbounded_channel::<(usize, mpsc::Sender<clhorde_core::protocol::DaemonEvent>)>();
    let (session_unregister_tx, mut session_unregister_rx) = mpsc::unbounded_channel::<usize>();

    let pty_byte_tx = orch.pty_byte_tx.clone();

    // 4. Spawn IPC server task
    let server_socket = socket_path.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = ipc_server::run_server(
            server_socket,
            cmd_tx,
            session_register_tx,
            session_unregister_tx,
            pty_byte_tx,
        )
        .await
        {
            error!("IPC server error: {e}");
        }
    });

    // 5. Signal handling
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM");
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to register SIGINT");

    // Dispatch any pending prompts from restored state
    orch.dispatch_workers();

    info!(
        pid = process::id(),
        socket = %socket_path.display(),
        "daemon started"
    );

    // 6. Main event loop
    let shutdown;
    loop {
        tokio::select! {
            // Worker messages from spawned processes
            msg = orch.worker_rx.recv() => {
                if let Some(msg) = msg {
                    orch.apply_message(msg);
                    orch.dispatch_workers();
                }
            }
            // Commands from clients
            cmd = cmd_rx.recv() => {
                if let Some(cmd) = cmd {
                    let is_shutdown = matches!(cmd.request, ClientRequest::Shutdown);
                    debug!(session_id = cmd.session_id, "client request received");
                    orch.handle_request(cmd.request, cmd.session_id);
                    if is_shutdown {
                        shutdown = true;
                        break;
                    }
                }
            }
            // New client registrations
            reg = session_register_rx.recv() => {
                if let Some((session_id, event_tx)) = reg {
                    info!(session_id, "client connected");
                    orch.sessions.add_session_with_id(session_id, event_tx);
                }
            }
            // Client disconnections
            unreg = session_unregister_rx.recv() => {
                if let Some(session_id) = unreg {
                    info!(session_id, "client disconnected");
                    orch.sessions.remove_session(session_id);
                }
            }
            // Signals
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                shutdown = true;
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
                shutdown = true;
                break;
            }
        }
    }

    // 7. Graceful shutdown
    if shutdown {
        info!("killing workers...");
        orch.shutdown();

        // Wait briefly for workers to exit
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while orch.active_workers > 0 && tokio::time::Instant::now() < deadline {
            tokio::select! {
                msg = orch.worker_rx.recv() => {
                    if let Some(msg) = msg {
                        orch.apply_message(msg);
                    } else {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    }

    server_handle.abort();
    cleanup_socket(&socket_path);
    info!("daemon stopped");
    // _pid_lock is dropped here, releasing the advisory lock
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_process_alive_self() {
        let pid = std::process::id() as i32;
        assert!(is_process_alive(pid));
    }

    #[test]
    fn is_process_alive_nonexistent() {
        // PID near i32::MAX is extremely unlikely to be in use
        assert!(!is_process_alive(i32::MAX - 1));
    }

    #[test]
    fn is_process_alive_pid_1() {
        // PID 1 (init/systemd) always exists; may return true via EPERM or 0
        assert!(is_process_alive(1));
    }

    #[test]
    fn acquire_pid_lock_succeeds_on_fresh_file() {
        let dir =
            std::env::temp_dir().join(format!("clhorde-pidlock-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let pid_path = dir.join("daemon.pid");

        let lock = acquire_pid_lock(&pid_path);
        assert!(
            lock.is_ok(),
            "acquire_pid_lock should succeed on fresh file"
        );

        // Verify PID was written
        let content = fs::read_to_string(&pid_path).unwrap();
        assert_eq!(content, format!("{}", process::id()));

        drop(lock);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquire_pid_lock_fails_when_already_held() {
        let dir =
            std::env::temp_dir().join(format!("clhorde-pidlock-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let pid_path = dir.join("daemon.pid");

        // Acquire the lock once
        let _lock1 = acquire_pid_lock(&pid_path).expect("first lock should succeed");

        // Second acquire should fail (same process, but LOCK_NB on same file)
        // Note: flock allows the same process to re-lock, so we use a child thread
        // with a separate file descriptor
        let pid_path_clone = pid_path.clone();
        let result = std::thread::spawn(move || acquire_pid_lock(&pid_path_clone))
            .join()
            .unwrap();

        // On Linux, flock is per-fd, not per-process, so a new fd from a new open()
        // will conflict even within the same process
        assert!(
            result.is_err(),
            "second lock should fail while first is held"
        );
        assert!(result.unwrap_err().contains("Daemon already running"));

        drop(_lock1);
        let _ = fs::remove_dir_all(&dir);
    }

    // ── arg parsing ──

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_no_flags_returns_warn() {
        let level = parse_args_from(&args(&[]));
        assert_eq!(level, Some(tracing::Level::WARN));
    }

    #[test]
    fn parse_args_v_returns_info() {
        let level = parse_args_from(&args(&["-v"]));
        assert_eq!(level, Some(tracing::Level::INFO));
    }

    #[test]
    fn parse_args_vv_returns_debug() {
        let level = parse_args_from(&args(&["-vv"]));
        assert_eq!(level, Some(tracing::Level::DEBUG));
    }

    #[test]
    fn parse_args_two_v_returns_debug() {
        let level = parse_args_from(&args(&["-v", "-v"]));
        assert_eq!(level, Some(tracing::Level::DEBUG));
    }

    #[test]
    fn parse_args_help_returns_none() {
        let level = parse_args_from(&args(&["--help"]));
        assert!(level.is_none());
    }

    #[test]
    fn parse_args_h_returns_none() {
        let level = parse_args_from(&args(&["-h"]));
        assert!(level.is_none());
    }
}
