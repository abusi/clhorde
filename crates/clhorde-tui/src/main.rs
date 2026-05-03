mod app;
mod cli;
mod editor;
mod ipc_client;
mod key_encoding;
mod keymap;
mod pty_renderer;
mod scheduler_client;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::App;
use clhorde_core::control::{ControlRequest, ControlResponse};
use clhorde_core::protocol::ClientRequest;
use cli::{CliAction, LaunchOptions};
use ipc_client::DaemonMessage;
use scheduler_client::{SchedulerError, SubscriptionMessage};

/// Backoff used when the scheduler subscribe stream drops. Long
/// enough that a stopped scheduler doesn't get hammered, short enough
/// that the user sees the lists fill back in within a couple seconds
/// of restarting `clhorde-scheduler daemon`.
const SCHEDULER_RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Result fed back from a spawned scheduler request. The main loop
/// consumes these and updates `App` state accordingly. With Phase 5.1
/// the periodic Status poll is gone — push events now arrive via a
/// dedicated `sched_event_rx` channel from the long-lived
/// subscription. This enum still routes the one-shot Q/X/T action
/// results and Detail responses.
//
// `Detail` carries a sizable payload (Vec + nested struct) while
// `ActionResult`/`DetailError` are small. Boxing the heavy variant would
// cost an extra allocation per request for no real benefit, since each value
// is produced and consumed once on the main task and never stored in a Vec.
#[allow(clippy::large_enum_variant)]
enum SchedulerPollOutcome {
    /// Result of a Q/X/T mutation. `ok = true` for `ControlResponse::Ok`,
    /// `false` for `Error`/network failure. The message is shown verbatim
    /// in the status line.
    ActionResult { ok: bool, message: String },
    /// A `Detail` response arrived; populate the detail overlay.
    Detail(clhorde_core::control::WorkflowDetail),
    /// A `Detail` request failed (scheduler unreachable, workflow
    /// disappeared, etc.). Carries the error message.
    DetailError(String),
}

/// Per-workflow detail subscription state. Held on the main task and
/// torn down whenever the open detail target changes.
///
/// `snapshot_seen` differentiates "the very first DetailSnapshot
/// hasn't landed yet" from "we got a snapshot, then mid-stream the
/// connection dropped." The first case triggers an unreachable toast
/// (the workflow likely doesn't exist or the scheduler is down); the
/// second case keeps the overlay open with stale data while the
/// reconnect tick re-spawns the subscription.
struct DetailSubscription {
    target: String,
    rx: mpsc::UnboundedReceiver<SubscriptionMessage>,
    snapshot_seen: bool,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let launch_opts = match cli::run(&args) {
        CliAction::Exit(code) => std::process::exit(code),
        CliAction::LaunchTui(opts) => opts,
    };

    // Connect to daemon before terminal setup so errors print cleanly
    let (daemon_tx, daemon_rx) = match ipc_client::connect().await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Failed to connect to clhorded daemon: {e}");
            eprintln!("Is the daemon running? Start it with: clhorded");
            std::process::exit(1);
        }
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, launch_opts, daemon_tx, daemon_rx).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }

    Ok(())
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    launch_opts: LaunchOptions,
    daemon_tx: mpsc::UnboundedSender<ClientRequest>,
    mut daemon_rx: mpsc::UnboundedReceiver<DaemonMessage>,
) -> io::Result<()> {
    let mut app = App::new(daemon_tx);

    // Subscribe and request initial state
    app.send_subscribe();
    app.send_get_state();

    // Submit prompt-from-files prompts
    let LaunchOptions {
        prompts,
        worktree,
        run_path,
    } = launch_opts;
    for text in prompts {
        app.add_prompt(text, run_path.clone(), worktree, Vec::new());
    }

    // Dedicated thread for crossterm event reading
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(ev) = event::read() {
                if event_tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));
    let mut reconnect_interval = tokio::time::interval(Duration::from_secs(2));

    // Channel for scheduler one-shot results (Q/X/T action results
    // and Detail responses).
    let (sched_tx, mut sched_rx) =
        mpsc::unbounded_channel::<SchedulerPollOutcome>();

    // Long-lived push subscription to the scheduler control socket.
    // The receiver feeds Snapshot + WorkflowUpdated events directly
    // into App. On disconnect we receive one Disconnected message and
    // then need to re-spawn the subscription after a backoff.
    let mut sched_event_rx = scheduler_client::subscribe();
    let mut sched_reconnect_interval =
        tokio::time::interval(SCHEDULER_RECONNECT_INTERVAL);
    sched_reconnect_interval
        .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sched_subscription_alive = true;

    // Per-workflow detail subscription, scoped to whatever workflow the
    // detail overlay is open on. Reconciled each tick against
    // `app.detail_subscription_target()` so opening / closing the
    // overlay or navigating between workflows transparently swaps the
    // underlying SubscribeDetail connection.
    let mut detail_sub: Option<DetailSubscription> = None;

    loop {
        terminal.draw(|f| ui::render(f, &mut app))?;

        // After draw: check if output panel size changed, resize PTY renderers + notify daemon
        if let Some(panel_size) = app.output_panel_size {
            if app.last_pty_size != Some(panel_size) && panel_size.0 > 0 && panel_size.1 > 0 {
                app.resize_pty_workers(panel_size.0, panel_size.1);
            }
        }

        tokio::select! {
            Some(ev) = event_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key);
                    }
                    Event::Paste(text) if app.mode == app::AppMode::Insert => {
                        for c in text.chars() {
                            if c == '\n' {
                                app.input.insert_newline();
                            } else if c != '\r' {
                                app.input.insert_char(c);
                            }
                        }
                    }
                    Event::Paste(text) if app.mode == app::AppMode::PtyInteract => {
                        app.paste_to_pty(&text);
                    }
                    Event::Paste(text) if app.mode == app::AppMode::Interact => {
                        app.interact_input.push_str(&text);
                    }
                    Event::Mouse(mouse) => {
                        app.handle_mouse(mouse);
                    }
                    Event::Resize(_, _) => {
                        // Terminal resized — next draw will update output_panel_size
                    }
                    _ => {}
                }
            }
            Some(msg) = daemon_rx.recv() => {
                match msg {
                    DaemonMessage::Event(event) => {
                        app.apply_event(*event);
                    }
                    DaemonMessage::PtyBytes { prompt_id, data } => {
                        app.apply_pty_bytes(prompt_id, &data);
                    }
                    DaemonMessage::Disconnected => {
                        app.connected = false;
                    }
                }
            }
            _ = tick_interval.tick() => {
                app.tick = app.tick.wrapping_add(1);
                app.clear_expired_status();

                // Drain Q/X/T/Detail action requests the user composed
                // since the previous tick. One spawn per request — tiny
                // cost, results land on the same `sched_rx` channel.
                for req in app.take_pending_scheduler_actions() {
                    let tx = sched_tx.clone();
                    tokio::spawn(async move {
                        dispatch_scheduler_action(req, tx).await;
                    });
                }

                // Reconcile the detail subscription against the open
                // overlay: if the user opened/closed/switched the
                // overlay since the previous tick, drop the stale
                // SubscribeDetail connection and spawn a fresh one
                // for the new target. Also covers the reconnect case:
                // a Disconnected message clears `detail_sub` while
                // leaving the overlay open, and the next tick re-spawns
                // the subscription against the same name.
                let desired = app.detail_subscription_target();
                let current = detail_sub.as_ref().map(|s| s.target.clone());
                if desired != current || (desired.is_some() && detail_sub.is_none()) {
                    detail_sub = desired.map(|name| DetailSubscription {
                        rx: scheduler_client::subscribe_detail(name.clone()),
                        target: name,
                        snapshot_seen: false,
                    });
                }
            }
            Some(msg) = sched_event_rx.recv() => {
                match msg {
                    SubscriptionMessage::Event(event) => {
                        app.apply_scheduler_event(event);
                    }
                    SubscriptionMessage::Disconnected(err) => {
                        // The subscription channel will keep returning
                        // None now until we replace it. Mark the
                        // scheduler unreachable so the UI shows the
                        // hint, and let the reconnect tick re-spawn it.
                        sched_subscription_alive = false;
                        app.note_scheduler_unreachable();
                        // Held for future diagnostics — logging into
                        // the status bar on every reconnect would
                        // clutter the UI during normal scheduler
                        // restarts.
                        let _ = err;
                    }
                }
            }
            // Per-workflow detail stream. The future yields `pending`
            // when no overlay is open so this arm contributes nothing
            // to wakeups in the common case. When an overlay opens,
            // the next tick installs `detail_sub` and this arm drives
            // the apply path.
            Some(msg) = async {
                match detail_sub.as_mut() {
                    Some(s) => s.rx.recv().await,
                    None => std::future::pending::<Option<SubscriptionMessage>>().await,
                }
            } => {
                match msg {
                    SubscriptionMessage::Event(event) => {
                        if matches!(event, clhorde_core::control::SchedulerEvent::DetailSnapshot { .. }) {
                            if let Some(sub) = detail_sub.as_mut() {
                                sub.snapshot_seen = true;
                            }
                        }
                        app.apply_detail_event(event);
                    }
                    SubscriptionMessage::Disconnected(err) => {
                        // Distinguish "the very first frame failed"
                        // (workflow doesn't exist, scheduler down) from
                        // "we had a stream and it dropped mid-way"
                        // (transient — preserve the overlay's last
                        // known state and let the reconcile pass spin
                        // up a fresh subscription).
                        let saw_snapshot = detail_sub
                            .as_ref()
                            .map(|s| s.snapshot_seen)
                            .unwrap_or(false);
                        detail_sub = None;
                        if !saw_snapshot {
                            app.note_detail_unreachable(err.to_string());
                        }
                    }
                }
            }
            Some(outcome) = sched_rx.recv() => {
                match outcome {
                    SchedulerPollOutcome::ActionResult { ok, message } => {
                        app.note_scheduler_action_result(ok, message);
                    }
                    SchedulerPollOutcome::Detail(detail) => {
                        app.apply_workflow_detail(detail);
                    }
                    SchedulerPollOutcome::DetailError(msg) => {
                        app.note_detail_unreachable(msg);
                    }
                }
            }
            _ = sched_reconnect_interval.tick(), if !sched_subscription_alive => {
                sched_event_rx = scheduler_client::subscribe();
                sched_subscription_alive = true;
            }
            _ = reconnect_interval.tick(), if !app.connected => {
                if let Ok((new_tx, new_rx)) = ipc_client::connect().await {
                    app.daemon_tx = new_tx;
                    daemon_rx = new_rx;
                    app.connected = true;
                    app.send_subscribe();
                    app.send_get_state();
                }
            }
        }

        // Check if user wants to open external editor
        if app.open_external_editor {
            app.open_external_editor = false;
            if let Err(e) = open_editor(terminal, &mut app) {
                app.status_message =
                    Some((format!("Editor error: {e}"), std::time::Instant::now()));
            }
        }

        // Check if user wants to open a file in $PAGER (R action on
        // Drafts/Workflows tabs).
        if let Some(path) = app.take_pending_pager_path() {
            if let Err(e) = open_pager(terminal, &path) {
                app.status_message =
                    Some((format!("Pager error: {e}"), std::time::Instant::now()));
            }
        }

        if app.should_quit {
            // TUI disconnects — daemon keeps running, workers continue
            return Ok(());
        }
    }
}

/// Dispatch a single [`ControlRequest`] over the scheduler control
/// socket and forward the result back through `tx`. Knows about
/// `Detail` (returns a [`SchedulerPollOutcome::Detail`] /
/// [`SchedulerPollOutcome::DetailError`]) and falls back to the
/// generic Ok/Error toast for every other request type.
async fn dispatch_scheduler_action(
    req: ControlRequest,
    tx: mpsc::UnboundedSender<SchedulerPollOutcome>,
) {
    let is_detail = matches!(req, ControlRequest::Detail { .. });
    let outcome = match scheduler_client::request(req).await {
        Ok(ControlResponse::Detail { detail }) => SchedulerPollOutcome::Detail(detail),
        Ok(ControlResponse::Ok { message }) => SchedulerPollOutcome::ActionResult {
            ok: true,
            message,
        },
        Ok(ControlResponse::Error { message }) => {
            if is_detail {
                SchedulerPollOutcome::DetailError(message)
            } else {
                SchedulerPollOutcome::ActionResult {
                    ok: false,
                    message,
                }
            }
        }
        Ok(_) => {
            if is_detail {
                SchedulerPollOutcome::DetailError("unexpected response".into())
            } else {
                SchedulerPollOutcome::ActionResult {
                    ok: false,
                    message: "unexpected response".into(),
                }
            }
        }
        Err(SchedulerError::Unreachable(_)) => {
            if is_detail {
                SchedulerPollOutcome::DetailError("not reachable".into())
            } else {
                SchedulerPollOutcome::ActionResult {
                    ok: false,
                    message: "not reachable".into(),
                }
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if is_detail {
                SchedulerPollOutcome::DetailError(msg)
            } else {
                SchedulerPollOutcome::ActionResult {
                    ok: false,
                    message: msg,
                }
            }
        }
    };
    let _ = tx.send(outcome);
}

/// Suspend the TUI, run `$PAGER <path>` (falling back to `less`/`more`),
/// then restore the terminal. Errors only on terminal-state failures —
/// a missing pager binary surfaces as a non-zero exit code, which is
/// reported through `status_message` by the caller.
fn open_pager(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &std::path::Path,
) -> io::Result<()> {
    let pager = std::env::var("PAGER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "less".to_string());

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;

    let _ = std::process::Command::new(&pager).arg(path).status();

    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    enable_raw_mode()?;
    terminal.clear()?;
    Ok(())
}

fn open_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let pid = std::process::id();
    let tmp_path = std::path::PathBuf::from(format!("/tmp/clhorde-prompt-{pid}.md"));

    // Write current input to temp file
    std::fs::write(&tmp_path, app.input.to_string())?;

    // Suspend terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;

    // Spawn editor
    let status = std::process::Command::new(&editor).arg(&tmp_path).status();

    // Restore terminal
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    enable_raw_mode()?;
    terminal.clear()?;

    match status {
        Ok(s) if s.success() => {
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            app.input.set(&content);
        }
        Ok(s) => {
            app.status_message = Some((
                format!("Editor exited with {}", s.code().unwrap_or(-1)),
                std::time::Instant::now(),
            ));
        }
        Err(e) => {
            app.status_message = Some((
                format!("Failed to run '{editor}': {e}"),
                std::time::Instant::now(),
            ));
        }
    }

    let _ = std::fs::remove_file(&tmp_path);
    Ok(())
}
