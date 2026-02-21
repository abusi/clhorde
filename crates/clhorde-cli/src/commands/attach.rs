use std::io::{self, Write};

use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::daemon_client;

pub fn cmd_attach(args: &[String]) -> i32 {
    let id: usize = match args.first().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => {
            eprintln!("Usage: clhorde-cli attach <id>");
            return 1;
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Runtime error: {e}");
            return 1;
        }
    };

    let result = rt.block_on(async {
        let setup = vec![ClientRequest::Subscribe, ClientRequest::GetState];

        let mut found = false;
        let mut exit_code: i32 = 0;

        daemon_client::stream_events(
            setup,
            |event| match event {
                DaemonEvent::StateSnapshot(state) => {
                    // Find the prompt; if completed/failed, request full output
                    match state.prompts.iter().find(|p| p.id == id) {
                        None => {
                            eprintln!("Prompt #{id} not found.");
                            exit_code = 1;
                            false
                        }
                        Some(info) => {
                            found = true;
                            if info.status == "Completed" || info.status == "Failed" {
                                // Print whatever output is available inline
                                if let Some(ref out) = info.output {
                                    print!("{out}");
                                    let _ = io::stdout().flush();
                                }
                                false // done
                            } else {
                                true // keep streaming
                            }
                        }
                    }
                }
                DaemonEvent::OutputChunk { prompt_id, text } if prompt_id == id => {
                    print!("{text}");
                    let _ = io::stdout().flush();
                    true
                }
                DaemonEvent::PromptOutput {
                    prompt_id,
                    full_text,
                } if prompt_id == id => {
                    print!("{full_text}");
                    let _ = io::stdout().flush();
                    false
                }
                DaemonEvent::WorkerFinished { prompt_id, .. } if prompt_id == id => {
                    false
                }
                DaemonEvent::WorkerError {
                    prompt_id, error, ..
                } if prompt_id == id => {
                    eprintln!("Worker error: {error}");
                    exit_code = 1;
                    false
                }
                DaemonEvent::Error { message } => {
                    eprintln!("Error: {message}");
                    exit_code = 1;
                    false
                }
                _ => true,
            },
            |prompt_id, data| {
                if prompt_id == id {
                    let _ = io::stdout().write_all(data);
                    let _ = io::stdout().flush();
                }
                true
            },
        )
        .await?;

        if !found {
            return Ok(exit_code);
        }

        Ok::<i32, String>(exit_code)
    });

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_no_args_returns_error() {
        assert_eq!(cmd_attach(&[]), 1);
    }

    #[test]
    fn attach_non_numeric_returns_error() {
        assert_eq!(cmd_attach(&["abc".into()]), 1);
    }
}
