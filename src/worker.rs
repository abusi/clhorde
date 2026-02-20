use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use tokio::sync::mpsc;

use crate::prompt::PromptMode;
use crate::pty_worker::PtyHandle;

#[allow(dead_code)]
pub enum WorkerMessage {
    OutputChunk { prompt_id: usize, text: String },
    TurnComplete { prompt_id: usize },
    Finished { prompt_id: usize, exit_code: Option<i32> },
    SpawnError { prompt_id: usize, error: String },
    PtyUpdate { #[allow(dead_code)] prompt_id: usize },
    SessionId { prompt_id: usize, session_id: String },
}

pub enum WorkerInput {
    SendInput(String),
    SendBytes(Vec<u8>),
    Kill,
}

/// Result of spawning a worker.
pub enum SpawnResult {
    /// Interactive PTY worker.
    Pty {
        input_sender: mpsc::UnboundedSender<WorkerInput>,
        pty_handle: PtyHandle,
    },
    /// One-shot worker (no follow-ups).
    OneShot,
    /// Spawn failed.
    Error(String),
}

/// Spawns a claude worker. For interactive mode, uses PTY when `pty_size` is
/// provided. For one-shot mode, uses stream-json as before.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    mode: PromptMode,
    tx: mpsc::UnboundedSender<WorkerMessage>,
    pty_size: Option<(u16, u16)>,
    resume_session_id: Option<String>,
    worktree: bool,
) -> SpawnResult {
    match mode {
        PromptMode::Interactive => {
            let (cols, rows) = pty_size.unwrap_or((80, 24));
            match crate::pty_worker::spawn_pty_worker(
                prompt_id,
                prompt_text,
                cwd,
                cols,
                rows,
                tx,
                resume_session_id,
                worktree,
            ) {
                Ok((input_sender, pty_handle)) => {
                    SpawnResult::Pty { input_sender, pty_handle }
                }
                Err(e) => SpawnResult::Error(e),
            }
        }
        PromptMode::OneShot => {
            spawn_oneshot(prompt_id, prompt_text, cwd, tx, resume_session_id, worktree);
            SpawnResult::OneShot
        }
    }
}

fn spawn_oneshot(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    tx: mpsc::UnboundedSender<WorkerMessage>,
    resume_session_id: Option<String>,
    worktree: bool,
) {
    std::thread::spawn(move || {
        let mut cmd = Command::new("claude");
        if worktree && resume_session_id.is_none() {
            cmd.args(["-w", &format!("clhorde-{prompt_id}")]);
        }
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
        if let Some(ref session_id) = resume_session_id {
            if session_id.is_empty() {
                cmd.arg("--resume");
            } else {
                cmd.args(["--resume", session_id]);
            }
        }
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
            read_stream_json(prompt_id, stdout, &reader_tx);
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

/// Parses stream-json lines from stdout, sends OutputChunk messages.
fn read_stream_json(
    prompt_id: usize,
    stdout: std::process::ChildStdout,
    tx: &mpsc::UnboundedSender<WorkerMessage>,
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

        // Capture session_id from init message
        if json["type"] == "system" {
            if let Some(session_id) = json["session_id"].as_str() {
                let _ = tx.send(WorkerMessage::SessionId {
                    prompt_id,
                    session_id: session_id.to_string(),
                });
            }
        }

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
    }
}
