use clhorde_core::protocol::{ClientRequest, DaemonEvent};

use crate::daemon_client;

const VALID_FILTERS: &[&str] = &["all", "completed", "failed", "pending", "running"];
const VALID_KEEP_FILTERS: &[&str] = &["completed", "failed", "pending", "running"];

pub fn cmd_store(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => store_list(),
        Some("count") => store_count(),
        Some("path") => store_path(),
        Some("drop") => store_drop(args.get(1).map(|s| s.as_str())),
        Some("keep") => store_keep(args.get(1).map(|s| s.as_str())),
        Some("clean-worktrees") => store_clean_worktrees(),
        _ => {
            eprintln!("Usage: clhorde-cli store <list|count|path|drop|keep|clean-worktrees>");
            eprintln!("  list              List all stored prompts");
            eprintln!("  count             Show prompt counts by state");
            eprintln!("  path              Print storage directory path");
            eprintln!("  drop <filter>     Delete stored prompts");
            eprintln!("  keep <filter>     Keep only matching, delete rest");
            eprintln!("  clean-worktrees   Remove lingering git worktrees");
            eprintln!();
            eprintln!("Filters: all, completed, failed, pending, running");
            1
        }
    }
}

/// Send a one-shot request to the daemon, return the response event.
fn daemon_request(req: ClientRequest) -> Result<DaemonEvent, String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("Runtime error: {e}"))?;
    rt.block_on(daemon_client::request(req))
}

fn store_path() -> i32 {
    match daemon_request(ClientRequest::StorePath) {
        Ok(DaemonEvent::StorePathResult { path }) => {
            println!("{path}");
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

fn store_list() -> i32 {
    match daemon_request(ClientRequest::StoreList) {
        Ok(DaemonEvent::StoreListResult { prompts }) => {
            if prompts.is_empty() {
                println!("No stored prompts.");
                return 0;
            }
            println!("{:<38} {:<11} {:<13} PROMPT", "UUID", "STATE", "MODE");
            println!("{}", "-".repeat(78));
            for info in &prompts {
                let text = if info.text.len() > 40 {
                    format!("{}...", &info.text[..37])
                } else {
                    info.text.clone()
                };
                let text = text.replace('\n', " ");
                println!(
                    "{:<38} {:<11} {:<13} {}",
                    info.uuid, info.status, info.mode, text
                );
            }
            println!("\n{} prompt(s) total.", prompts.len());
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

fn store_count() -> i32 {
    match daemon_request(ClientRequest::StoreCount) {
        Ok(DaemonEvent::StoreCountResult {
            pending,
            running,
            completed,
            failed,
        }) => {
            let total = pending + running + completed + failed;
            if total == 0 {
                println!("No stored prompts.");
                return 0;
            }
            if completed > 0 {
                println!("completed: {completed}");
            }
            if failed > 0 {
                println!("failed: {failed}");
            }
            if pending > 0 {
                println!("pending: {pending}");
            }
            if running > 0 {
                println!("running: {running}");
            }
            println!("total: {total}");
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

fn store_drop(filter: Option<&str>) -> i32 {
    let filter = match filter {
        Some(f) => f,
        None => {
            eprintln!("Usage: clhorde-cli store drop <filter>");
            eprintln!("Filters: all, completed, failed, pending, running");
            return 1;
        }
    };

    if !VALID_FILTERS.contains(&filter) {
        eprintln!("Unknown filter: {filter}");
        eprintln!("Valid filters: all, completed, failed, pending, running");
        return 1;
    }

    match daemon_request(ClientRequest::StoreDrop {
        filter: filter.to_string(),
    }) {
        Ok(DaemonEvent::StoreOpComplete { message }) => {
            println!("{message}");
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

fn store_keep(filter: Option<&str>) -> i32 {
    let filter = match filter {
        Some(f) => f,
        None => {
            eprintln!("Usage: clhorde-cli store keep <filter>");
            eprintln!("Filters: completed, failed, pending, running");
            return 1;
        }
    };

    if !VALID_KEEP_FILTERS.contains(&filter) {
        eprintln!("Unknown filter: {filter}");
        eprintln!("Valid filters: completed, failed, pending, running");
        return 1;
    }

    match daemon_request(ClientRequest::StoreKeep {
        filter: filter.to_string(),
    }) {
        Ok(DaemonEvent::StoreOpComplete { message }) => {
            println!("{message}");
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

fn store_clean_worktrees() -> i32 {
    match daemon_request(ClientRequest::CleanWorktrees) {
        Ok(DaemonEvent::StoreOpComplete { message }) => {
            println!("{message}");
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
    fn store_subcommand_no_args_returns_error() {
        assert_eq!(cmd_store(&[]), 1);
    }

    #[test]
    fn store_drop_no_filter_returns_error() {
        assert_eq!(store_drop(None), 1);
    }

    #[test]
    fn store_drop_invalid_filter_returns_error() {
        assert_eq!(store_drop(Some("bogus")), 1);
    }

    #[test]
    fn store_keep_no_filter_returns_error() {
        assert_eq!(store_keep(None), 1);
    }

    #[test]
    fn store_keep_invalid_filter_returns_error() {
        assert_eq!(store_keep(Some("bogus")), 1);
    }

    #[test]
    fn store_keep_all_not_valid() {
        // "all" is valid for drop but not for keep
        assert_eq!(store_keep(Some("all")), 1);
    }
}
