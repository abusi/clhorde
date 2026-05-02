//! `clhorde-scheduler` binary entrypoint.
//!
//! - `daemon` runs the long-lived watcher (FS + daemon events drive the
//!   orchestrator).
//! - Every other subcommand is a one-shot wrapper around a function in
//!   [`clhorde_scheduler::commands`]. The bodies live there so they can be
//!   unit-tested without spawning a process.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use clhorde_scheduler::cli::{Cli, Command, DaemonArgs, TemplatesAction};
use clhorde_scheduler::commands::{self, CommandError, CommandOutput};
use clhorde_scheduler::daemon_client::{self, DaemonMessage};
use clhorde_scheduler::orchestrator::Orchestrator;
use clhorde_scheduler::persistence::WorkflowStore;
use clhorde_scheduler::watcher::{self, FsEvent};
use clhorde_core::protocol::ClientRequest;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref());

    match cli.command {
        Command::Daemon(args) => run_daemon(args).await,
        Command::Queue(args) => run_one_shot(commands::queue(args)),
        Command::Unqueue(args) => {
            run_one_shot(commands::unqueue(args, None))
        }
        Command::Drafts(args) => run_one_shot(commands::drafts(args)),
        Command::Status(args) => match WorkflowStore::open_default() {
            Ok(store) => run_one_shot(commands::status(args, &store)),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Command::Templates(args) => match args.action {
            TemplatesAction::Path => run_one_shot(commands::templates_path()),
            TemplatesAction::Edit => run_one_shot(commands::templates_edit(None)),
        },
        Command::Cancel(args) => match WorkflowStore::open_default() {
            Ok(store) => run_one_shot(commands::cancel(args, None, &store)),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
        Command::Apply(args) => run_one_shot(commands::apply(args).await),
        Command::Archive(args) => {
            run_one_shot(commands::archive(args, None).await)
        }
        Command::Propose(args) => run_one_shot(commands::propose(args).await),
        Command::Retry(args) => match WorkflowStore::open_default() {
            Ok(store) => {
                run_one_shot(commands::retry(args, None, &store).await)
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(1)
            }
        },
    }
}

/// Adapt a `Result<CommandOutput, CommandError>` to an `ExitCode`. Stdout is
/// printed verbatim; errors go to stderr and yield `1`.
fn run_one_shot(result: Result<CommandOutput, CommandError>) -> ExitCode {
    match result {
        Ok(out) => {
            if !out.stdout.is_empty() {
                print!("{}", out.stdout);
            }
            if !out.stderr.is_empty() {
                eprint!("{}", out.stderr);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn init_tracing(override_filter: Option<&str>) {
    let filter = match override_filter {
        Some(s) => EnvFilter::try_new(s).unwrap_or_else(|_| EnvFilter::new("info")),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

/// `daemon` subcommand: reconcile against disk, spawn the FS watcher, then
/// keep a long-lived daemon connection open while applying watcher events
/// to the orchestrator. Reconnects to the daemon with backoff; the watcher
/// stays up across daemon disconnects.
async fn run_daemon(args: DaemonArgs) -> ExitCode {
    info!("clhorde-scheduler daemon starting");

    let root = match resolve_root(args.root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Cannot resolve scheduler root: {e}");
            return ExitCode::from(1);
        }
    };
    info!(root = %root.display(), "scheduler root");

    let store = match WorkflowStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open workflow store: {e}");
            return ExitCode::from(1);
        }
    };
    let (orch_tx, mut orch_rx) = mpsc::unbounded_channel::<ClientRequest>();
    let mut orch = Orchestrator::new(root.clone(), store, orch_tx);
    if let Err(e) = orch.reconcile() {
        warn!(error = %e, "initial reconcile failed; continuing");
    }

    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<FsEvent>();
    let _watcher_handle = match watcher::spawn(root.clone(), fs_tx) {
        Ok(h) => Some(h),
        Err(e) => {
            // Watcher failure is recoverable — the daemon connection still
            // works, the user just doesn't get reactive workflow updates
            // until the next manual `apply`. Log and proceed.
            warn!(
                error = %e,
                "filesystem watcher could not start; reactive workflow updates disabled"
            );
            None
        }
    };

    loop {
        match daemon_client::connect().await {
            Ok((tx, mut rx)) => {
                if tx.send(ClientRequest::Subscribe).is_err() {
                    warn!("scheduler: failed to send Subscribe; reconnecting");
                    sleep_backoff().await;
                    continue;
                }
                info!("scheduler connected to daemon, subscribed");

                loop {
                    tokio::select! {
                        msg = rx.recv() => match msg {
                            Some(DaemonMessage::Event(ev)) => {
                                tracing::trace!(?ev, "daemon event");
                                if let Err(e) = orch.handle_daemon_event(&ev) {
                                    warn!(error = %e, "orchestrator daemon event failed");
                                }
                            }
                            Some(DaemonMessage::Disconnected) | None => {
                                warn!("scheduler: daemon disconnected, reconnecting");
                                break;
                            }
                        },
                        ev = fs_rx.recv() => match ev {
                            Some(ev) => {
                                tracing::debug!(?ev, "fs event");
                                if let Err(e) = orch.handle_event(ev) {
                                    warn!(error = %e, "orchestrator fs event failed");
                                }
                            }
                            None => {
                                warn!("scheduler: fs watcher channel closed");
                            }
                        },
                        req = orch_rx.recv() => match req {
                            Some(req) => {
                                if tx.send(req).is_err() {
                                    warn!("scheduler: daemon writer closed; reconnecting");
                                    break;
                                }
                            }
                            None => {
                                // Should never happen — orch_tx is held by Orchestrator.
                                warn!("scheduler: orchestrator outbound channel closed");
                                return ExitCode::SUCCESS;
                            }
                        },
                        _ = tokio::signal::ctrl_c() => {
                            info!("scheduler: SIGINT received, shutting down");
                            return ExitCode::SUCCESS;
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "scheduler: cannot connect to daemon — is it running?");
            }
        }

        // Drain any FS events that arrived while we were disconnected so we
        // don't lose them on the next reconnect. Outbound requests stay
        // buffered in the channel for the next connection to forward.
        while let Ok(ev) = fs_rx.try_recv() {
            if let Err(e) = orch.handle_event(ev) {
                warn!(error = %e, "orchestrator event failed during reconnect");
            }
        }

        tokio::select! {
            _ = sleep_backoff() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("scheduler: SIGINT received during reconnect wait");
                return ExitCode::SUCCESS;
            }
        }
    }
}

fn resolve_root(arg: Option<PathBuf>) -> std::io::Result<PathBuf> {
    match arg {
        Some(p) => Ok(p),
        None => std::env::current_dir(),
    }
}

async fn sleep_backoff() {
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}
