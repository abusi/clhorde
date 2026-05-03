use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::pty_worker::PtyHandle;
use clhorde_core::prompt::PromptMode;

#[derive(Debug)]
#[allow(dead_code)]
pub enum WorkerMessage {
    OutputChunk {
        prompt_id: usize,
        text: String,
    },
    TurnComplete {
        prompt_id: usize,
    },
    Finished {
        prompt_id: usize,
        exit_code: Option<i32>,
    },
    SpawnError {
        prompt_id: usize,
        error: String,
    },
    PtyUpdate {
        #[allow(dead_code)]
        prompt_id: usize,
    },
    SessionId {
        prompt_id: usize,
        session_id: String,
    },
    /// PTY reader hit EOF — child process output is done but we haven't reaped it yet.
    PtyEof {
        prompt_id: usize,
    },
    /// Async worktree creation completed (spawned in a background thread).
    WorktreeCreated {
        prompt_id: usize,
        result: Result<String, String>,
    },
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

/// Fields common to spawning either a PTY or one-shot worker.
pub struct WorkerSpawn {
    pub prompt_id: usize,
    pub prompt_text: String,
    pub cwd: Option<String>,
    pub tx: mpsc::Sender<WorkerMessage>,
    pub resume_session_id: Option<String>,
    pub session_id: Option<String>,
    pub pty_byte_tx: tokio::sync::broadcast::Sender<(usize, Vec<u8>)>,
}

/// Spawns a claude worker. For interactive mode, uses PTY when `pty_size` is
/// provided. For one-shot mode, uses stream-json as before.
pub fn spawn_worker(
    req: WorkerSpawn,
    mode: PromptMode,
    pty_size: Option<(u16, u16)>,
) -> SpawnResult {
    let prompt_id = req.prompt_id;
    match mode {
        PromptMode::Interactive => {
            debug!(prompt_id, "spawning PTY worker");
            let (cols, rows) = pty_size.unwrap_or((80, 24));
            match crate::pty_worker::spawn_pty_worker(req, cols, rows) {
                Ok((input_sender, pty_handle)) => SpawnResult::Pty {
                    input_sender,
                    pty_handle,
                },
                Err(e) => SpawnResult::Error(e),
            }
        }
        PromptMode::OneShot => {
            debug!(prompt_id, "spawning one-shot worker");
            spawn_oneshot(req);
            SpawnResult::OneShot
        }
    }
}

fn spawn_oneshot(req: WorkerSpawn) {
    let WorkerSpawn {
        prompt_id,
        prompt_text,
        cwd,
        tx,
        resume_session_id,
        session_id,
        pty_byte_tx: _,
    } = req;
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
        if let Some(ref sid) = resume_session_id {
            cmd.arg("--resume");
            if !sid.is_empty() {
                cmd.args(["--session-id", sid]);
            }
        } else if let Some(ref sid) = session_id {
            cmd.args(["--session-id", sid]);
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
                error!(prompt_id, error = %e, "failed to spawn claude");
                let _ = tx.blocking_send(WorkerMessage::SpawnError {
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

        let _ = tx.blocking_send(WorkerMessage::Finished {
            prompt_id,
            exit_code,
        });
    });
}

/// Parses stream-json lines from stdout, sends OutputChunk messages.
fn read_stream_json<R: std::io::Read>(
    prompt_id: usize,
    stdout: R,
    tx: &mpsc::Sender<WorkerMessage>,
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
                let _ = tx.blocking_send(WorkerMessage::SessionId {
                    prompt_id,
                    session_id: session_id.to_string(),
                });
            }
        }

        // Extract streaming text deltas
        if json["type"] == "stream_event" {
            if let Some(text) = json["event"]["delta"]["text"].as_str() {
                if !text.is_empty() {
                    let _ = tx.blocking_send(WorkerMessage::OutputChunk {
                        prompt_id,
                        text: text.to_string(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Helper: run read_stream_json on input lines and collect all WorkerMessages.
    fn parse_lines(input: &str) -> Vec<WorkerMessage> {
        let (tx, mut rx) = mpsc::channel(64);
        let cursor = Cursor::new(input.to_string().into_bytes());
        read_stream_json(0, cursor, &tx);
        drop(tx);
        let mut msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    #[test]
    fn extracts_session_id_from_system_message() {
        let input = r#"{"type":"system","session_id":"sess-abc-123"}"#;
        let msgs = parse_lines(&format!("{input}\n"));
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            WorkerMessage::SessionId {
                prompt_id,
                session_id,
            } => {
                assert_eq!(*prompt_id, 0);
                assert_eq!(session_id, "sess-abc-123");
            }
            other => panic!("expected SessionId, got {other:?}"),
        }
    }

    #[test]
    fn extracts_text_delta_from_stream_event() {
        let input = r#"{"type":"stream_event","event":{"delta":{"text":"hello world"}}}"#;
        let msgs = parse_lines(&format!("{input}\n"));
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            WorkerMessage::OutputChunk { prompt_id, text } => {
                assert_eq!(*prompt_id, 0);
                assert_eq!(text, "hello world");
            }
            other => panic!("expected OutputChunk, got {other:?}"),
        }
    }

    #[test]
    fn skips_empty_lines() {
        let input = "\n\n\n";
        let msgs = parse_lines(input);
        assert!(msgs.is_empty());
    }

    #[test]
    fn skips_malformed_json() {
        let input = "not json at all\n{broken\n";
        let msgs = parse_lines(input);
        assert!(msgs.is_empty());
    }

    #[test]
    fn skips_empty_text_delta() {
        let input = r#"{"type":"stream_event","event":{"delta":{"text":""}}}"#;
        let msgs = parse_lines(&format!("{input}\n"));
        assert!(msgs.is_empty());
    }

    #[test]
    fn ignores_unknown_event_types() {
        let input = r#"{"type":"unknown","data":"whatever"}"#;
        let msgs = parse_lines(&format!("{input}\n"));
        assert!(msgs.is_empty());
    }

    #[test]
    fn system_message_without_session_id_is_ignored() {
        let input = r#"{"type":"system","version":"1.0"}"#;
        let msgs = parse_lines(&format!("{input}\n"));
        assert!(msgs.is_empty());
    }

    #[test]
    fn stream_event_without_delta_is_ignored() {
        let input = r#"{"type":"stream_event","event":{"kind":"start"}}"#;
        let msgs = parse_lines(&format!("{input}\n"));
        assert!(msgs.is_empty());
    }

    #[test]
    fn multiple_messages_in_sequence() {
        let lines = [
            r#"{"type":"system","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"delta":{"text":"one"}}}"#,
            r#"{"type":"stream_event","event":{"delta":{"text":"two"}}}"#,
        ];
        let input = lines.join("\n") + "\n";
        let msgs = parse_lines(&input);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(&msgs[0], WorkerMessage::SessionId { session_id, .. } if session_id == "s1"));
        assert!(matches!(&msgs[1], WorkerMessage::OutputChunk { text, .. } if text == "one"));
        assert!(matches!(&msgs[2], WorkerMessage::OutputChunk { text, .. } if text == "two"));
    }

    #[test]
    fn mixed_valid_and_invalid_lines() {
        let lines = [
            "garbage",
            r#"{"type":"stream_event","event":{"delta":{"text":"ok"}}}"#,
            "{bad json",
            r#"{"type":"system","session_id":"s2"}"#,
        ];
        let input = lines.join("\n") + "\n";
        let msgs = parse_lines(&input);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], WorkerMessage::OutputChunk { text, .. } if text == "ok"));
        assert!(matches!(&msgs[1], WorkerMessage::SessionId { session_id, .. } if session_id == "s2"));
    }

    #[test]
    fn uses_correct_prompt_id() {
        let (tx, mut rx) = mpsc::channel(64);
        let input = r#"{"type":"stream_event","event":{"delta":{"text":"hi"}}}"#;
        let cursor = Cursor::new(format!("{input}\n").into_bytes());
        read_stream_json(42, cursor, &tx);
        drop(tx);
        match rx.try_recv().unwrap() {
            WorkerMessage::OutputChunk { prompt_id, .. } => assert_eq!(prompt_id, 42),
            other => panic!("expected OutputChunk, got {other:?}"),
        }
    }
}
