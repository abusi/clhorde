use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::daemon_client;

pub fn cmd_status(args: &[String]) -> i32 {
    if !args.is_empty() {
        eprintln!("Usage: clhorde-cli status");
        return 1;
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Runtime error: {e}");
            return 1;
        }
    };

    match rt.block_on(daemon_client::request(ClientRequest::GetState)) {
        Ok(DaemonEvent::StateSnapshot(state)) => {
            println!(
                "Workers: {}/{} active",
                state.active_workers, state.max_workers
            );
            println!("Mode: {}", state.default_mode);
            println!();

            if state.prompts.is_empty() {
                println!("No prompts.");
                return 0;
            }

            println!(
                "{:<4} {:<14} {:<13} PROMPT",
                "ID", "STATUS", "MODE"
            );
            println!("{}", "-".repeat(70));
            for info in &state.prompts {
                let text = if info.text.len() > 40 {
                    format!("{}...", &info.text[..37])
                } else {
                    info.text.clone()
                };
                let text = text.replace('\n', " ");
                let status_display = format!(
                    "{} {}",
                    info.status_symbol(),
                    info.status.to_lowercase()
                );
                println!(
                    "{:<4} {:<14} {:<13} {}",
                    info.id, status_display, info.mode, text
                );
            }
            0
        }
        Ok(DaemonEvent::Error { message }) => {
            eprintln!("Error: {message}");
            1
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
        _ => {
            eprintln!("Unexpected response from daemon.");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_with_args_returns_error() {
        assert_eq!(cmd_status(&["extra".into()]), 1);
    }
}
