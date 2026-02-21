use clhorde_core::persistence;
use clhorde_core::worktree;

const VALID_STATES: &[&str] = &["completed", "failed", "pending", "running"];

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

fn store_dir_or_err() -> Result<std::path::PathBuf, i32> {
    match persistence::default_prompts_dir() {
        Some(d) => Ok(d),
        None => {
            eprintln!("Cannot determine storage directory.");
            Err(1)
        }
    }
}

fn store_path() -> i32 {
    match store_dir_or_err() {
        Ok(d) => {
            println!("{}", d.display());
            0
        }
        Err(code) => code,
    }
}

fn store_list() -> i32 {
    let dir = match store_dir_or_err() {
        Ok(d) => d,
        Err(code) => return code,
    };
    let prompts = persistence::load_all_prompts(&dir);
    if prompts.is_empty() {
        println!("No stored prompts.");
        return 0;
    }
    println!(
        "{:<38} {:<11} {:<13} PROMPT",
        "UUID", "STATE", "MODE"
    );
    println!("{}", "-".repeat(78));
    for (uuid, p) in &prompts {
        let text = if p.prompt.len() > 40 {
            format!("{}...", &p.prompt[..37])
        } else {
            p.prompt.clone()
        };
        // Replace newlines with spaces for display
        let text = text.replace('\n', " ");
        println!(
            "{:<38} {:<11} {:<13} {}",
            uuid, p.state, p.options.mode, text
        );
    }
    println!("\n{} prompt(s) total.", prompts.len());
    0
}

fn store_count() -> i32 {
    let dir = match store_dir_or_err() {
        Ok(d) => d,
        Err(code) => return code,
    };
    let prompts = persistence::load_all_prompts(&dir);
    if prompts.is_empty() {
        println!("No stored prompts.");
        return 0;
    }

    let mut counts = std::collections::HashMap::new();
    for (_, p) in &prompts {
        *counts.entry(p.state.as_str()).or_insert(0usize) += 1;
    }
    for state in VALID_STATES {
        if let Some(&n) = counts.get(state) {
            println!("{state}: {n}");
        }
    }
    println!("total: {}", prompts.len());
    0
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

    if filter != "all" && !VALID_STATES.contains(&filter) {
        eprintln!("Unknown filter: {filter}");
        eprintln!("Valid filters: all, completed, failed, pending, running");
        return 1;
    }

    let dir = match store_dir_or_err() {
        Ok(d) => d,
        Err(code) => return code,
    };

    if filter == "all" {
        let prompts = persistence::load_all_prompts(&dir);
        let count = prompts.len();
        for (uuid, _) in &prompts {
            persistence::delete_prompt_file(&dir, uuid);
        }
        println!("Dropped {count} prompt(s).");
    } else {
        let prompts = persistence::load_all_prompts(&dir);
        let mut count = 0;
        for (uuid, p) in &prompts {
            if p.state == filter {
                persistence::delete_prompt_file(&dir, uuid);
                count += 1;
            }
        }
        println!("Dropped {count} {filter} prompt(s).");
    }
    0
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

    if !VALID_STATES.contains(&filter) {
        eprintln!("Unknown filter: {filter}");
        eprintln!("Valid filters: completed, failed, pending, running");
        return 1;
    }

    let dir = match store_dir_or_err() {
        Ok(d) => d,
        Err(code) => return code,
    };
    let prompts = persistence::load_all_prompts(&dir);
    let mut dropped = 0;
    let mut kept = 0;
    for (uuid, p) in &prompts {
        if p.state != filter {
            persistence::delete_prompt_file(&dir, uuid);
            dropped += 1;
        } else {
            kept += 1;
        }
    }
    println!("Kept {kept} {filter} prompt(s), dropped {dropped}.");
    0
}

fn store_clean_worktrees() -> i32 {
    let dir = match store_dir_or_err() {
        Ok(d) => d,
        Err(code) => return code,
    };
    let prompts = persistence::load_all_prompts(&dir);
    let mut cleaned = 0;
    let mut skipped = 0;
    let mut errors = 0;

    for (uuid, pf) in &prompts {
        let Some(ref wt_path_str) = pf.worktree_path else {
            continue;
        };
        let wt_path = std::path::Path::new(wt_path_str);
        if !wt_path.exists() {
            println!("  skip (already gone): {wt_path_str}");
            skipped += 1;
            // Clear the worktree_path in the persisted file
            let updated = persistence::PromptFile {
                prompt: pf.prompt.clone(),
                options: persistence::PromptOptions {
                    mode: pf.options.mode.clone(),
                    context: pf.options.context.clone(),
                    worktree: pf.options.worktree,
                },
                state: pf.state.clone(),
                queue_rank: pf.queue_rank,
                session_id: pf.session_id.clone(),
                worktree_path: None,
                tags: pf.tags.clone(),
            };
            persistence::save_prompt(&dir, uuid, &updated);
            continue;
        }

        // Try to find the repo root to run git worktree remove
        let mut removed = false;
        if let Some(parent) = wt_path.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && path != wt_path {
                        if let Some(root) = worktree::repo_root(&path) {
                            match worktree::remove_worktree(&root, wt_path) {
                                Ok(()) => {
                                    println!("  removed: {wt_path_str}");
                                    cleaned += 1;
                                    removed = true;
                                    // Clear worktree_path in persisted file
                                    let updated = persistence::PromptFile {
                                        prompt: pf.prompt.clone(),
                                        options: persistence::PromptOptions {
                                            mode: pf.options.mode.clone(),
                                            context: pf.options.context.clone(),
                                            worktree: pf.options.worktree,
                                        },
                                        state: pf.state.clone(),
                                        queue_rank: pf.queue_rank,
                                        session_id: pf.session_id.clone(),
                                        worktree_path: None,
                                        tags: pf.tags.clone(),
                                    };
                                    persistence::save_prompt(&dir, uuid, &updated);
                                    break;
                                }
                                Err(e) => {
                                    eprintln!("  error: {wt_path_str}: {e}");
                                    errors += 1;
                                    removed = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !removed {
            eprintln!("  error: {wt_path_str}: could not find parent git repo");
            errors += 1;
        }
    }

    let total = cleaned + skipped;
    if total == 0 && errors == 0 {
        println!("No worktrees to clean.");
    } else {
        println!("Cleaned {cleaned} worktree(s), {skipped} already gone, {errors} error(s).");
    }
    if errors > 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use clhorde_core::persistence::{PromptFile, PromptOptions};

    fn temp_store_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clhorde-cli-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_prompt(state: &str, rank: f64) -> PromptFile {
        PromptFile {
            prompt: format!("test {state}"),
            options: PromptOptions {
                mode: "interactive".to_string(),
                context: None,
                worktree: None,
            },
            state: state.to_string(),
            queue_rank: rank,
            session_id: None,
            worktree_path: None,
            tags: Vec::new(),
        }
    }

    fn seed_store(dir: &std::path::Path, states: &[&str]) -> Vec<String> {
        let mut uuids = Vec::new();
        for (i, state) in states.iter().enumerate() {
            let uuid = uuid::Uuid::now_v7().to_string();
            persistence::save_prompt(dir, &uuid, &make_prompt(state, i as f64));
            uuids.push(uuid);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        uuids
    }

    #[test]
    fn store_subcommand_no_args_returns_error() {
        assert_eq!(cmd_store(&[]), 1);
    }

    #[test]
    fn store_path_returns_ok() {
        assert_eq!(store_path(), 0);
    }

    #[test]
    fn store_list_empty() {
        assert_eq!(store_list(), 0);
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
    fn store_drop_all_clears_directory() {
        let dir = temp_store_dir();
        seed_store(&dir, &["completed", "failed", "pending"]);
        assert_eq!(persistence::load_all_prompts(&dir).len(), 3);

        let prompts = persistence::load_all_prompts(&dir);
        for (uuid, _) in &prompts {
            persistence::delete_prompt_file(&dir, uuid);
        }
        assert_eq!(persistence::load_all_prompts(&dir).len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_drop_by_state_filters_correctly() {
        let dir = temp_store_dir();
        seed_store(&dir, &["completed", "failed", "completed", "pending"]);

        let prompts = persistence::load_all_prompts(&dir);
        for (uuid, p) in &prompts {
            if p.state == "completed" {
                persistence::delete_prompt_file(&dir, uuid);
            }
        }
        let remaining = persistence::load_all_prompts(&dir);
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|(_, p)| p.state != "completed"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_keep_by_state_filters_correctly() {
        let dir = temp_store_dir();
        seed_store(&dir, &["completed", "failed", "completed", "pending"]);

        let prompts = persistence::load_all_prompts(&dir);
        for (uuid, p) in &prompts {
            if p.state != "completed" {
                persistence::delete_prompt_file(&dir, uuid);
            }
        }
        let remaining = persistence::load_all_prompts(&dir);
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|(_, p)| p.state == "completed"));
        let _ = fs::remove_dir_all(&dir);
    }
}
