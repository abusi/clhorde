mod app;
mod cli;
mod editor;
mod keymap;
mod persistence;
mod prompt;
mod pty_worker;
mod ui;
mod worker;
mod worktree;

use std::io;
use std::time::Duration;

use crossterm::event::{self, EnableBracketedPaste, DisableBracketedPaste, Event, KeyEventKind};
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
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen)?;
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
                let mut cwd = prompt.cwd.clone();
                let mode = prompt.mode;
                let wants_worktree = prompt.worktree;
                let resume_session_id = if prompt.resume {
                    Some(prompt.session_id.clone().unwrap_or_default())
                } else {
                    None
                };

                // Create git worktree if requested
                if wants_worktree {
                    let effective_cwd = cwd.as_deref()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    match worktree::repo_root(&effective_cwd) {
                        Some(root) => {
                            match worktree::create_worktree(&root, id) {
                                Ok(wt_path) => {
                                    let wt_str = wt_path.to_string_lossy().to_string();
                                    cwd = Some(wt_str.clone());
                                    if let Some(p) = app.prompts.get_mut(idx) {
                                        p.worktree_path = Some(wt_str);
                                    }
                                }
                                Err(e) => {
                                    app.mark_running(idx);
                                    app.active_workers += 1;
                                    app.apply_message(WorkerMessage::SpawnError {
                                        prompt_id: id,
                                        error: format!("Worktree creation failed: {e}"),
                                    });
                                    continue;
                                }
                            }
                        }
                        None => {
                            app.mark_running(idx);
                            app.active_workers += 1;
                            app.apply_message(WorkerMessage::SpawnError {
                                prompt_id: id,
                                error: "Not inside a git repository — cannot create worktree".to_string(),
                            });
                            continue;
                        }
                    }
                }

                app.mark_running(idx);
                app.active_workers += 1;
                let pty_size = app.output_panel_size;
                match worker::spawn_worker(id, text, cwd, mode, worker_tx.clone(), pty_size, resume_session_id)
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

        // Check if user wants to open external editor
        if app.open_external_editor {
            app.open_external_editor = false;
            if let Err(e) = open_editor(terminal, &mut app) {
                app.status_message = Some((format!("Editor error: {e}"), std::time::Instant::now()));
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
    execute!(terminal.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen)?;

    // Spawn editor
    let status = std::process::Command::new(&editor)
        .arg(&tmp_path)
        .status();

    // Restore terminal
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableBracketedPaste)?;
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
