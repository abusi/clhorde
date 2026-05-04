//! `clhorde-scheduler` binary entrypoint.
//!
//! - `daemon` runs the long-lived watcher (FS + daemon events drive the
//!   orchestrator).
//! - Every other subcommand is a one-shot wrapper around a function in
//!   [`clhorde_scheduler::commands`]. The bodies live there so they can be
//!   unit-tested without spawning a process.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use clhorde_scheduler::cli::{Cli, Command, DaemonArgs, TemplatesAction};
use clhorde_scheduler::commands::{self, CommandError, CommandOutput};
use clhorde_scheduler::control;
use clhorde_scheduler::daemon_client::{self, DaemonMessage};
use clhorde_scheduler::orchestrator::Orchestrator;
use clhorde_scheduler::persistence::WorkflowStore;
use clhorde_scheduler::jira;
use clhorde_scheduler::source;
use clhorde_scheduler::watcher::{self, FsEvent};
use clhorde_core::control::{ControlRequest, ControlResponse};
use clhorde_core::ipc::scheduler_socket_path;
use clhorde_core::keymap;
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
            Ok(store) => {
                let result = commands::status(args, &store).map(|mut out| {
                    // Best-effort: ask the running daemon for source
                    // health and append it. Skipped silently when the
                    // daemon is offline (the workflow list still
                    // renders from disk).
                    if let Some(reports) = fetch_source_health() {
                        out.stdout.push_str(&commands::format_source_health(&reports));
                    }
                    out
                });
                run_one_shot(result)
            }
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
    let orch = Arc::new(Mutex::new(Orchestrator::new(
        root.clone(),
        store,
        orch_tx,
    )));
    let jira_validation = validate_jira_config();
    let enable_jira = jira_validation.is_some();
    {
        let mut g = orch.lock().expect("orchestrator mutex poisoned");
        // Register every default source so the status surface lists
        // them even before their first event lands. Jira is registered
        // when the user has a valid `[sources.jira]` block in
        // `keymap.toml`; queues that fail validation (missing fields,
        // direct mode, etc.) are skipped with logged errors but do not
        // disable the whole source.
        source::register_default_sources(&mut g, enable_jira);
        if let Err(e) = g.reconcile() {
            warn!(error = %e, "initial reconcile failed; continuing");
        }
    }
    // The validated config is dropped here in this change — the actual
    // poll loop / writeback wiring happens in follow-up sections that
    // build on top of section 8. Holding it briefly until the source
    // is registered is enough to satisfy the "validate at scheduler
    // startup" contract for this change.
    drop(jira_validation);

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
    // Tracks whether the watcher's sender side is still alive. Hoisted out of
    // the inner select loop so a closed channel disables the arm for the rest
    // of the daemon's lifetime instead of spinning on `recv() -> None`.
    let mut watcher_alive = _watcher_handle.is_some();

    // Spawn the control socket so `clhorde-cli flow status` (and Phase 4's
    // TUI) can talk to this scheduler instance. Failure to bind is
    // non-fatal — the daemon flow still works without remote control.
    let control_socket = scheduler_socket_path();
    let _control_handle = match control::server::spawn(orch.clone(), control_socket.clone()) {
        Ok(h) => Some(h),
        Err(e) => {
            warn!(
                error = %e,
                socket = %control_socket.display(),
                "control socket bind failed; remote control disabled",
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
                                let mut g = orch.lock().expect("orch mutex");
                                if let Err(e) = g.handle_daemon_event(&ev) {
                                    warn!(error = %e, "orchestrator daemon event failed");
                                }
                            }
                            Some(DaemonMessage::Disconnected) | None => {
                                warn!("scheduler: daemon disconnected, reconnecting");
                                break;
                            }
                        },
                        ev = fs_rx.recv(), if watcher_alive => match ev {
                            Some(ev) => {
                                tracing::debug!(?ev, "fs event");
                                let mut g = orch.lock().expect("orch mutex");
                                if let Err(e) = g.handle_event(ev) {
                                    warn!(error = %e, "orchestrator fs event failed");
                                }
                            }
                            None => {
                                // Sender dropped (watcher thread exited).
                                // Flip the gate so this arm stops being
                                // polled — otherwise select! would race here
                                // every iteration and burn CPU logging.
                                warn!("scheduler: fs watcher channel closed; reactive updates disabled");
                                watcher_alive = false;
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
                                cleanup_control_socket(&control_socket);
                                return ExitCode::SUCCESS;
                            }
                        },
                        _ = tokio::signal::ctrl_c() => {
                            info!("scheduler: SIGINT received, shutting down");
                            cleanup_control_socket(&control_socket);
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
            let mut g = orch.lock().expect("orch mutex");
            if let Err(e) = g.handle_event(ev) {
                warn!(error = %e, "orchestrator event failed during reconnect");
            }
        }

        tokio::select! {
            _ = sleep_backoff() => {}
            _ = tokio::signal::ctrl_c() => {
                info!("scheduler: SIGINT received during reconnect wait");
                cleanup_control_socket(&control_socket);
                return ExitCode::SUCCESS;
            }
        }
    }
}

fn cleanup_control_socket(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Best-effort lookup of per-source health from the running scheduler.
///
/// Returns `Some(reports)` when the daemon is reachable AND its Status
/// response carries source health data. Returns `None` on any failure
/// (no daemon running, timeout, decoding mismatch) — the caller is
/// expected to treat absence as "no extra info to render", not as an
/// error worth surfacing. Synchronous wrapper around the async control
/// client so the existing one-shot `Status` command stays sync.
fn fetch_source_health() -> Option<Vec<clhorde_core::control::SourceHealthReport>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let result = rt.block_on(control::client::request(ControlRequest::Status {
        name: None,
    }));
    match result {
        Ok(ControlResponse::Status { source_health, .. }) if !source_health.is_empty() => {
            Some(source_health)
        }
        _ => None,
    }
}

/// Validate the user's `[sources.jira]` block at scheduler startup.
///
/// Returns `Some(JiraConfig)` when the source is configured AND every
/// source-wide required field is present; per-queue errors (e.g.
/// `mode = "direct"`, missing `filter_jql`) are logged but do not
/// disable the whole source. Returns `None` when the block is absent
/// or the source-wide validation hard-fails — in either case the daemon
/// proceeds without registering Jira and the OpenSpec source continues
/// to operate normally.
fn validate_jira_config() -> Option<jira::JiraConfig> {
    let toml_config = keymap::load_toml_config();
    let jira_toml = toml_config.sources.and_then(|s| s.jira)?;
    match jira::build_config_partial(&jira_toml) {
        Ok(outcome) => {
            if outcome.config.poll_interval_clamped {
                warn!(
                    floor_secs = jira::MIN_POLL_INTERVAL.as_secs(),
                    "[sources.jira] poll_interval_secs below floor; clamped to {}s",
                    jira::MIN_POLL_INTERVAL.as_secs(),
                );
            }
            for skip in &outcome.skipped_queue_errors {
                warn!(error = %skip, "[sources.jira] queue skipped");
            }
            info!(
                queues = outcome.config.queues.len(),
                "Jira source configured; registering",
            );
            Some(outcome.config)
        }
        Err(errs) => {
            for e in &errs {
                tracing::error!(error = %e, "[sources.jira] invalid config — source disabled");
            }
            None
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
