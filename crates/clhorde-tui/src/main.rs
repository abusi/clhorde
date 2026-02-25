mod app;
mod cli;
mod editor;
mod ipc_client;
mod key_encoding;
mod keymap;
mod pty_renderer;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::App;
use clhorde_core::protocol::ClientRequest;
use cli::{CliAction, LaunchOptions};
use ipc_client::DaemonMessage;

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
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, launch_opts, daemon_tx, daemon_rx).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
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

        if app.should_quit {
            // TUI disconnects — daemon keeps running, workers continue
            return Ok(());
        }
    }
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
        LeaveAlternateScreen
    )?;

    // Spawn editor
    let status = std::process::Command::new(&editor).arg(&tmp_path).status();

    // Restore terminal
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste
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
