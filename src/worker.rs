use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use tokio::sync::mpsc;

use crate::prompt::PromptMode;

pub enum WorkerMessage {
    OutputChunk { prompt_id: usize, text: String },
    TurnComplete { prompt_id: usize },
    Finished { prompt_id: usize, exit_code: Option<i32> },
    SpawnError { prompt_id: usize, error: String },
}

pub enum WorkerInput {
    SendInput(String),
    Kill,
}

fn format_user_message(text: &str) -> String {
    let content = serde_json::to_string(text).unwrap_or_else(|_| format!("\"{}\"", text));
    format!(r#"{{"type":"user","message":{{"role":"user","content":{content}}}}}"#)
}

/// Spawns a claude worker. Returns `Some(input_sender)` for interactive mode,
/// `None` for one-shot mode (no follow-ups possible).
pub fn spawn_worker(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    mode: PromptMode,
    tx: mpsc::UnboundedSender<WorkerMessage>,
) -> Option<mpsc::UnboundedSender<WorkerInput>> {
    match mode {
        PromptMode::Interactive => Some(spawn_interactive(prompt_id, prompt_text, cwd, tx)),
        PromptMode::OneShot => {
            spawn_oneshot(prompt_id, prompt_text, cwd, tx);
            None
        }
    }
}

fn spawn_interactive(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    tx: mpsc::UnboundedSender<WorkerMessage>,
) -> mpsc::UnboundedSender<WorkerInput> {
    let (input_tx, input_rx) = mpsc::unbounded_channel::<WorkerInput>();

    std::thread::spawn(move || {
        let mut cmd = Command::new("claude");
        cmd.args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--dangerously-skip-permissions",
            ])
            .env_remove("CLAUDECODE");
        if let Some(ref dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let _ = tx.send(WorkerMessage::SpawnError {
                    prompt_id,
                    error: format!("Failed to spawn claude: {e}"),
                });
                return;
            }
        };

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Send initial prompt
        let initial_msg = format_user_message(&prompt_text);
        if writeln!(stdin, "{initial_msg}").is_err() {
            let _ = tx.send(WorkerMessage::SpawnError {
                prompt_id,
                error: "Failed to write initial prompt to stdin".to_string(),
            });
            return;
        }
        let _ = stdin.flush();

        // Reader thread: parse JSON lines from stdout, extract text deltas
        let reader_tx = tx.clone();
        let reader_handle = std::thread::spawn(move || {
            read_stream_json(prompt_id, stdout, &reader_tx, true);
        });

        // Writer thread: receive WorkerInput, format as JSON, write to stdin
        let writer_handle = std::thread::spawn(move || {
            let mut input_rx = input_rx;
            while let Some(msg) = input_rx.blocking_recv() {
                match msg {
                    WorkerInput::SendInput(text) => {
                        let json_msg = format_user_message(&text);
                        if writeln!(stdin, "{json_msg}").is_err() {
                            break;
                        }
                        let _ = stdin.flush();
                    }
                    WorkerInput::Kill => {
                        // Drop stdin to signal EOF — claude will exit
                        break;
                    }
                }
            }
            // stdin is dropped here, signaling EOF to claude
        });

        // Wait for child to exit (blocks until claude finishes)
        let exit_code = match child.wait() {
            Ok(status) => status.code(),
            Err(_) => Some(1),
        };

        let _ = reader_handle.join();
        let _ = writer_handle.join();

        let _ = tx.send(WorkerMessage::Finished {
            prompt_id,
            exit_code,
        });
    });

    input_tx
}

fn spawn_oneshot(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    tx: mpsc::UnboundedSender<WorkerMessage>,
) {
    std::thread::spawn(move || {
        let mut cmd = Command::new("claude");
        cmd.args(["-p"])
            .arg(&prompt_text)
            .args([
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--dangerously-skip-permissions",
            ])
            .env_remove("CLAUDECODE");
        if let Some(ref dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = match cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let _ = tx.send(WorkerMessage::SpawnError {
                    prompt_id,
                    error: format!("Failed to spawn claude: {e}"),
                });
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();

        // Reader thread: parse JSON lines from stdout, extract text deltas
        let reader_tx = tx.clone();
        let reader_handle = std::thread::spawn(move || {
            read_stream_json(prompt_id, stdout, &reader_tx, false);
        });

        let exit_code = match child.wait() {
            Ok(status) => status.code(),
            Err(_) => Some(1),
        };

        let _ = reader_handle.join();

        let _ = tx.send(WorkerMessage::Finished {
            prompt_id,
            exit_code,
        });
    });
}

/// Shared stdout reader: parses stream-json lines, sends OutputChunk messages.
/// If `emit_turn_complete` is true, also sends TurnComplete on "result" events.
fn read_stream_json(
    prompt_id: usize,
    stdout: std::process::ChildStdout,
    tx: &mpsc::UnboundedSender<WorkerMessage>,
    emit_turn_complete: bool,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }

        let json: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Extract streaming text deltas
        if json["type"] == "stream_event" {
            if let Some(text) = json["event"]["delta"]["text"].as_str() {
                if !text.is_empty() {
                    let _ = tx.send(WorkerMessage::OutputChunk {
                        prompt_id,
                        text: text.to_string(),
                    });
                }
            }
        }

        // Detect turn completion (interactive mode only)
        if emit_turn_complete && json["type"] == "result" {
            let _ = tx.send(WorkerMessage::TurnComplete { prompt_id });
        }
    }
}
