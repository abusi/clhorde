use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::daemon_client;

pub fn cmd_submit(args: &[String]) -> i32 {
    let mut text: Option<String> = None;
    let mut mode = "interactive".to_string();
    let mut cwd: Option<String> = None;
    let mut worktree = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--mode requires a value (interactive or one-shot)");
                    return 1;
                }
                match args[i].as_str() {
                    "interactive" | "one-shot" => mode = args[i].clone(),
                    other => {
                        eprintln!("Unknown mode: {other}");
                        eprintln!("Valid modes: interactive, one-shot");
                        return 1;
                    }
                }
            }
            "--cwd" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--cwd requires a path");
                    return 1;
                }
                cwd = Some(args[i].clone());
            }
            "--worktree" => {
                worktree = true;
            }
            _ => {
                if text.is_none() && !args[i].starts_with('-') {
                    text = Some(args[i].clone());
                } else {
                    eprintln!("Unexpected argument: {}", args[i]);
                    return 1;
                }
            }
        }
        i += 1;
    }

    let text = match text {
        Some(t) => t,
        None => {
            eprintln!(
                "Usage: clhorde-cli submit \"prompt text\" [--mode interactive|one-shot] [--cwd path] [--worktree]"
            );
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
        // Subscribe first so we receive events, then submit
        let setup = vec![
            ClientRequest::Subscribe,
            ClientRequest::SubmitPrompt {
                text,
                cwd,
                mode,
                worktree,
                tags: vec![],
            },
        ];

        let mut exit_code = 1;
        let timeout = tokio::time::Duration::from_secs(5);

        tokio::time::timeout(timeout, daemon_client::stream_events(
            setup,
            |event| {
                match event {
                    DaemonEvent::PromptAdded(info) => {
                        println!("Submitted prompt #{}", info.id);
                        exit_code = 0;
                        false // stop
                    }
                    DaemonEvent::Error { message } => {
                        eprintln!("Error: {message}");
                        false
                    }
                    _ => true, // keep waiting
                }
            },
            |_prompt_id, _data| true, // ignore PTY bytes
        ))
        .await
        .map_err(|_| "Timeout waiting for response".to_string())
        .and_then(|r| r)?;

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
    fn submit_no_args_returns_error() {
        assert_eq!(cmd_submit(&[]), 1);
    }

    #[test]
    fn submit_invalid_mode_returns_error() {
        assert_eq!(
            cmd_submit(&[
                "hello".into(),
                "--mode".into(),
                "bogus".into(),
            ]),
            1
        );
    }

    #[test]
    fn submit_mode_missing_value_returns_error() {
        assert_eq!(cmd_submit(&["hello".into(), "--mode".into()]), 1);
    }

    #[test]
    fn submit_cwd_missing_value_returns_error() {
        assert_eq!(cmd_submit(&["hello".into(), "--cwd".into()]), 1);
    }
}
