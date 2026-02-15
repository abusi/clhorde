mod app;
mod keymap;
mod prompt;
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
use worker::{WorkerInput, WorkerMessage};

#[tokio::main]
async fn main() -> io::Result<()> {
    let restore = std::env::args().any(|a| a == "--restore");

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, restore).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }

    Ok(())
}

async fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, restore: bool) -> io::Result<()> {
    let mut app = App::new();

    if restore {
        app.load_session();
    }

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

        // Dispatch pending prompts to workers
        while app.active_workers < app.max_workers {
            if let Some(idx) = app.next_pending_prompt_index() {
                let prompt = &app.prompts[idx];
                let id = prompt.id;
                let text = prompt.text.clone();
                let cwd = prompt.cwd.clone();
                let mode = prompt.mode;
                app.mark_running(idx);
                app.active_workers += 1;
                if let Some(input_sender) =
                    worker::spawn_worker(id, text, cwd, mode, worker_tx.clone())
                {
                    app.worker_inputs.insert(id, input_sender);
                }
            } else {
                break;
            }
        }

        tokio::select! {
            Some(ev) = event_rx.recv() => {
                if let Event::Key(key) = ev {
                    // Only handle key press events (not release/repeat)
                    if key.kind == KeyEventKind::Press {
                        app.handle_key(key);
                    }
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
            // Save session before quitting
            app.save_session();

            // Send Kill to all active workers
            for (_id, sender) in app.worker_inputs.drain() {
                let _ = sender.send(WorkerInput::Kill);
            }
            // Brief sleep for cleanup
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Ok(());
        }
    }
}
