mod ipc_server;
mod orchestrator;
mod pty_worker;
mod session;
mod worker;

use std::fs;
use std::path::PathBuf;
use std::process;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

use clhorde_core::ipc::{daemon_pid_path, daemon_socket_path};
use clhorde_core::protocol::ClientRequest;

fn check_pid_file(pid_path: &PathBuf) -> Result<(), String> {
    if pid_path.exists() {
        let content = fs::read_to_string(pid_path).unwrap_or_default();
        if let Ok(pid) = content.trim().parse::<i32>() {
            // Check if the process is alive
            unsafe {
                if libc::kill(pid, 0) == 0 {
                    return Err(format!("Daemon already running (PID {pid})"));
                }
            }
        }
        // Stale PID file — clean up
        let _ = fs::remove_file(pid_path);
        let socket_path = daemon_socket_path();
        let _ = fs::remove_file(&socket_path);
    }
    Ok(())
}

fn write_pid_file(pid_path: &PathBuf) -> Result<(), String> {
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create dir: {e}"))?;
    }
    fs::write(pid_path, format!("{}", process::id()))
        .map_err(|e| format!("Failed to write PID file: {e}"))
}

fn cleanup_files(pid_path: &PathBuf, socket_path: &PathBuf) {
    let _ = fs::remove_file(pid_path);
    let _ = fs::remove_file(socket_path);
}

#[tokio::main]
async fn main() {
    let pid_path = daemon_pid_path();
    let socket_path = daemon_socket_path();

    // 1. PID file protocol
    if let Err(e) = check_pid_file(&pid_path) {
        eprintln!("clhorded: {e}");
        process::exit(1);
    }
    if let Err(e) = write_pid_file(&pid_path) {
        eprintln!("clhorded: {e}");
        process::exit(1);
    }

    // Clean up any stale socket
    let _ = fs::remove_file(&socket_path);

    // 2. Create orchestrator
    let mut orch = orchestrator::Orchestrator::new();

    // 3. Channels for IPC server <-> main loop
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ipc_server::ServerCommand>();
    let (session_register_tx, mut session_register_rx) =
        mpsc::unbounded_channel::<(usize, mpsc::UnboundedSender<clhorde_core::protocol::DaemonEvent>)>();
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
            eprintln!("IPC server error: {e}");
        }
    });

    // 5. Signal handling
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM");
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to register SIGINT");

    // Dispatch any pending prompts from restored state
    orch.dispatch_workers();

    eprintln!(
        "clhorded: started (PID {}, socket {})",
        process::id(),
        socket_path.display()
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
                    orch.sessions.add_session_with_id(session_id, event_tx);
                }
            }
            // Client disconnections
            unreg = session_unregister_rx.recv() => {
                if let Some(session_id) = unreg {
                    orch.sessions.remove_session(session_id);
                }
            }
            // Signals
            _ = sigterm.recv() => {
                eprintln!("clhorded: received SIGTERM, shutting down...");
                shutdown = true;
                break;
            }
            _ = sigint.recv() => {
                eprintln!("clhorded: received SIGINT, shutting down...");
                shutdown = true;
                break;
            }
        }
    }

    // 7. Graceful shutdown
    if shutdown {
        eprintln!("clhorded: killing workers...");
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
    cleanup_files(&pid_path, &socket_path);
    eprintln!("clhorded: stopped");
}
