mod app;
mod cli;
mod keymap;
mod persistence;
mod prompt;
mod pty_worker;
mod ui;
mod worker;


use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::App;
use worker::{SpawnResult, WorkerInput, WorkerMessage};

#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(code) = cli::run(&args) {
        std::process::exit(code);
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }

    Ok(())
}

async fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::new();

    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel::<WorkerMessage>();

    // Dedicated thread for crossterm event reading
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    if event_tx.send(ev).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| ui::render(f, &mut app))?;

        // After draw: check if output panel size changed, resize PTY workers
        if let Some(panel_size) = app.output_panel_size {
            if app.last_pty_size != Some(panel_size) && panel_size.0 > 0 && panel_size.1 > 0 {
                app.resize_pty_workers(panel_size.0, panel_size.1);
            }
        }

        // Dispatch pending prompts to workers
        while app.active_workers < app.max_workers {
            if let Some(idx) = app.next_pending_prompt_index() {
                let prompt = &app.prompts[idx];
                let id = prompt.id;
                let text = prompt.text.clone();
                let cwd = prompt.cwd.clone();
                let mode = prompt.mode;
                let wants_worktree = prompt.worktree;
                let resume_session_id = if prompt.resume {
                    Some(prompt.session_id.clone().unwrap_or_default())
                } else {
                    None
                };

                app.mark_running(idx);
                app.active_workers += 1;
                let pty_size = app.output_panel_size;
                match worker::spawn_worker(id, text, cwd, mode, worker_tx.clone(), pty_size, resume_session_id, wants_worktree)
                {
                    SpawnResult::Pty {
                        input_sender,
                        pty_handle,
                    } => {
                        app.worker_inputs.insert(id, input_sender);
                        // Store PTY state on the prompt
                        if let Some(p) = app.prompts.iter_mut().find(|p| p.id == id) {
                            p.pty_state = Some(pty_handle.state.clone());
                        }
                        app.pty_handles.insert(id, pty_handle);
                    }
                    SpawnResult::OneShot => {
                        // No input sender for one-shot
                    }
                    SpawnResult::Error(e) => {
                        app.apply_message(WorkerMessage::SpawnError {
                            prompt_id: id,
                            error: e,
                        });
                    }
                }
            } else {
                break;
            }
        }

        tokio::select! {
            Some(ev) = event_rx.recv() => {
                match ev {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        app.handle_key(key);
                    }
                    Event::Resize(_, _) => {
                        // Terminal resized — next draw will update output_panel_size
                        // and resize_pty_workers will be called
                    }
                    _ => {}
                }
            }
            Some(msg) = worker_rx.recv() => {
                app.apply_message(msg);
            }
            _ = tick_interval.tick() => {
                app.tick = app.tick.wrapping_add(1);
                app.clear_expired_status();
            }
        }

        if app.should_quit {
            // Send Kill to all active workers
            for (_id, sender) in app.worker_inputs.drain() {
                let _ = sender.send(WorkerInput::Kill);
            }
            // Clear PTY handles (drops masters → children get SIGHUP)
            app.pty_handles.clear();
            // Brief sleep for cleanup
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Ok(());
        }
    }
}
