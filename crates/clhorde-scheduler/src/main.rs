//! `clhorde-scheduler` binary entrypoint.
//!
//! Phase 2.1 wires up the CLI and the long-lived `daemon` subcommand to the
//! daemon over IPC. Subscribe + log incoming events; idle until SIGINT.
//! Workflow logic (FS watcher, dispatch, archiving) lands in 2.2+.

use std::process::ExitCode;

use clap::Parser;
use clhorde_scheduler::cli::{Cli, Command};
use clhorde_scheduler::daemon_client::{self, DaemonMessage};
use clhorde_core::protocol::ClientRequest;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log.as_deref());

    match cli.command {
        Command::Daemon(_) => run_daemon().await,
        Command::Apply(_)
        | Command::Archive(_)
        | Command::Cancel(_)
        | Command::Drafts(_)
        | Command::Propose(_)
        | Command::Queue(_)
        | Command::Retry(_)
        | Command::Status(_)
        | Command::Templates(_)
        | Command::Unqueue(_) => {
            eprintln!("This subcommand is not implemented yet (Phase 2.2+).");
            ExitCode::from(2)
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

/// `daemon` subcommand: connect, subscribe, idle until SIGINT, reconnecting
/// when the daemon goes away.
async fn run_daemon() -> ExitCode {
    info!("clhorde-scheduler daemon starting");

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
                                tracing::debug!(?ev, "daemon event");
                            }
                            Some(DaemonMessage::Disconnected) | None => {
                                warn!("scheduler: daemon disconnected, reconnecting");
                                break;
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

        // Wait before retrying so we don't spin if the daemon is down.
        tokio::select! {
            _ = sleep_backoff() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("scheduler: SIGINT received during reconnect wait");
                return ExitCode::SUCCESS;
            }
        }
    }
}

async fn sleep_backoff() {
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}
