//! Wire framing and socket path resolution for IPC.
//! Stub — will be populated in Phase 3 (daemon implementation).

use std::path::PathBuf;

/// Default daemon socket path: `~/.local/share/clhorde/daemon.sock`
pub fn daemon_socket_path() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp/clhorde"))
        .join("daemon.sock")
}

/// Default daemon PID file path: `~/.local/share/clhorde/daemon.pid`
pub fn daemon_pid_path() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp/clhorde"))
        .join("daemon.pid")
}
