use std::collections::HashMap;

use crossterm::event::KeyCode;

use crate::keymap::{
    self, FilterAction, InsertAction, InteractAction, Keymap, NormalAction, TomlConfig,
    TomlFilterBindings, TomlInsertBindings, TomlInteractBindings, TomlNormalBindings,
    TomlViewBindings, ViewAction,
};
use clhorde_core::persistence;
use clhorde_core::worktree;

pub struct LaunchOptions {
    pub prompts: Vec<String>,
    pub worktree: bool,
    pub run_path: Option<String>,
}

pub enum CliAction {
    Exit(i32),
    LaunchTui(LaunchOptions),
}

pub fn run(args: &[String]) -> CliAction {
    let Some(cmd) = args.get(1).map(|s| s.as_str()) else {
        return CliAction::LaunchTui(LaunchOptions { prompts: vec![], worktree: false, run_path: None });
    };
    match cmd {
        "help" | "--help" | "-h" => CliAction::Exit(cmd_help()),
        "qp" => CliAction::Exit(cmd_qp(&args[2..])),
        "keys" => CliAction::Exit(cmd_keys(&args[2..])),
        "config" => CliAction::Exit(cmd_config(&args[2..])),
        "store" => CliAction::Exit(cmd_store(&args[2..])),
        "prompt-from-files" => cmd_prompt_from_files(&args[2..]),
        _ => CliAction::LaunchTui(LaunchOptions { prompts: vec![], worktree: false, run_path: None }),
    }
}

fn cmd_help() -> i32 {
    println!("clhorde {}", env!("CARGO_PKG_VERSION"));
    println!("A TUI for orchestrating multiple Claude Code CLI instances in parallel.");
    println!();
    println!("Usage: clhorde [command] [options]");
    println!();
    println!("Commands:");
    println!("  (none)              Launch the TUI");
    println!("  store               Manage persisted prompts");
    println!("    list              List all stored prompts");
    println!("    count             Show prompt counts by state");
    println!("    path              Print storage directory path");
    println!("    drop <filter>     Delete stored prompts");
    println!("    keep <filter>     Keep only matching, delete rest");
    println!("    clean-worktrees   Remove lingering git worktrees");
    println!("  qp                  Manage quick prompts");
    println!("    list              List all quick prompts");
    println!("    add <key> <msg>   Add a quick prompt");
    println!("    remove <key>      Remove a quick prompt");
    println!("  keys                Manage keybindings");
    println!("    list [mode]       List keybindings (all or by mode)");
    println!("    set <mode> <action> <key1...>");
    println!("                      Set keys for an action");
    println!("    reset <mode> [action]");
    println!("                      Reset bindings to defaults");
    println!("  config              Manage config file");
    println!("    path              Print config file path");
    println!("    edit              Open config in $EDITOR");
    println!("    init [--force]    Create config with defaults");
    println!("  prompt-from-files [--run-path <path>] <files...>");
    println!("                      Load prompts from files and launch TUI");
    println!("                      Each prompt runs in its own git worktree");
    println!("                      --run-path sets the working directory for all prompts");
    println!();
    println!("Modes: normal, insert, view, interact, filter");
    println!();
    println!("Filters for drop/keep: all, completed, failed, pending");
    println!();
    println!("Examples:");
    println!("  clhorde store list");
    println!("  clhorde store drop all");
    println!("  clhorde store drop failed");
    println!("  clhorde store keep completed");
    println!("  clhorde qp add g \"let's go\"");
    println!("  clhorde keys set normal quit Q");
    println!("  clhorde keys list normal");
    println!("  clhorde config init");
    println!("  clhorde prompt-from-files tasks/*.md");
    println!("  clhorde prompt-from-files --run-path /tmp/myproject tasks/*.md");
    println!("  clhorde prompt-from-files a.txt,b.txt c.txt");
    0
}

// ── qp subcommands ──

fn cmd_qp(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => qp_list(),
        Some("add") => qp_add(&args[1..]),
        Some("remove") => qp_remove(&args[1..]),
        _ => {
            eprintln!("Usage: clhorde qp <list|add|remove>");
            eprintln!("  list              List all quick prompts");
            eprintln!("  add <key> <msg>   Add a quick prompt");
            eprintln!("  remove <key>      Remove a quick prompt");
            1
        }
    }
}

fn qp_list() -> i32 {
    let config = keymap::load_toml_config();
    match config.quick_prompts {
        Some(ref qp) if !qp.is_empty() => {
            let mut entries: Vec<_> = qp.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (key, message) in entries {
                println!("{key} = \"{message}\"");
            }
        }
        _ => println!("No quick prompts configured."),
    }
    0
}

fn qp_add(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("Usage: clhorde qp add <key> <message...>");
        return 1;
    }
    let key_str = &args[0];
    if keymap::parse_key(key_str).is_none() {
        eprintln!("Invalid key: {key_str}");
        eprintln!("Valid keys: single characters (a-z, A-Z, 0-9, symbols) or Enter, Esc, Tab, Space, Up, Down, Left, Right, Backspace");
        return 1;
    }
    let message = args[1..].join(" ");

    let mut config = keymap::load_toml_config();
    let qp = config.quick_prompts.get_or_insert_with(HashMap::new);
    qp.insert(key_str.clone(), message.clone());

    if let Err(e) = keymap::save_toml_config(&config) {
        eprintln!("Failed to save config: {e}");
        return 1;
    }
    println!("Added quick prompt: {key_str} = \"{message}\"");
    0
}

fn qp_remove(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: clhorde qp remove <key>");
        return 1;
    }
    let key_str = &args[0];

    let mut config = keymap::load_toml_config();
    let removed = config
        .quick_prompts
        .as_mut()
        .and_then(|qp| qp.remove(key_str));
    if removed.is_none() {
        eprintln!("Quick prompt '{key_str}' not found.");
        return 1;
    }

    if let Err(e) = keymap::save_toml_config(&config) {
        eprintln!("Failed to save config: {e}");
        return 1;
    }
    println!("Removed quick prompt: {key_str}");
    0
}

// ── prompt-from-files ──

fn cmd_prompt_from_files(args: &[String]) -> CliAction {
    if args.is_empty() {
        eprintln!("Usage: clhorde prompt-from-files [--run-path <path>] <file1> [file2...] or <file1,file2,...>");
        return CliAction::Exit(1);
    }

    // Extract --run-path <path> from args
    let mut run_path: Option<String> = None;
    let mut file_args: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--run-path" {
            if let Some(path) = args.get(i + 1) {
                let p = std::path::Path::new(path);
                if !p.exists() {
                    eprintln!("Error: --run-path does not exist: {path}");
                    return CliAction::Exit(1);
                }
                if !p.is_dir() {
                    eprintln!("Error: --run-path is not a directory: {path}");
                    return CliAction::Exit(1);
                }
                run_path = Some(path.clone());
                i += 2;
            } else {
                eprintln!("Error: --run-path requires a path argument");
                return CliAction::Exit(1);
            }
        } else {
            file_args.push(&args[i]);
            i += 1;
        }
    }

    if file_args.is_empty() {
        eprintln!("Usage: clhorde prompt-from-files [--run-path <path>] <file1> [file2...] or <file1,file2,...>");
        return CliAction::Exit(1);
    }

    let mut prompts = Vec::new();
    for arg in &file_args {
        for path_str in arg.split(',') {
            let path_str = path_str.trim();
            if path_str.is_empty() {
                continue;
            }
            match std::fs::read_to_string(path_str) {
                Ok(content) => {
                    let content = content.trim().to_string();
                    if content.is_empty() {
                        eprintln!("Warning: skipping empty file: {path_str}");
                    } else {
                        prompts.push(content);
                    }
                }
                Err(e) => {
                    eprintln!("Warning: cannot read {path_str}: {e}");
                }
            }
        }
    }

    if prompts.is_empty() {
        eprintln!("No valid prompts loaded from files.");
        return CliAction::Exit(1);
    }

    CliAction::LaunchTui(LaunchOptions { prompts, worktree: true, run_path })
}

// ── store subcommands ──

const VALID_STATES: &[&str] = &["completed", "failed", "pending", "running"];

fn cmd_store(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => store_list(),
        Some("count") => store_count(),
        Some("path") => store_path(),
        Some("drop") => store_drop(args.get(1).map(|s| s.as_str())),
        Some("keep") => store_keep(args.get(1).map(|s| s.as_str())),
        Some("clean-worktrees") => store_clean_worktrees(),
        _ => {
            eprintln!("Usage: clhorde store <list|count|path|drop|keep|clean-worktrees>");
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
            eprintln!("Usage: clhorde store drop <filter>");
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
            eprintln!("Usage: clhorde store keep <filter>");
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

// ── keys subcommands ──

fn cmd_keys(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => keys_list(args.get(1).map(|s| s.as_str())),
        Some("set") => keys_set(&args[1..]),
        Some("reset") => keys_reset(&args[1..]),
        _ => {
            eprintln!("Usage: clhorde keys <list|set|reset>");
            eprintln!("  list [mode]                     List keybindings");
            eprintln!("  set <mode> <action> <key1...>   Set keybinding");
            eprintln!("  reset <mode> [action]           Reset to defaults");
            1
        }
    }
}

fn keys_list(mode: Option<&str>) -> i32 {
    let km = Keymap::load();

    match mode {
        Some("normal") => print_mode_bindings("normal", &invert_normal(&km)),
        Some("insert") => print_mode_bindings("insert", &invert_insert(&km)),
        Some("view") => print_mode_bindings("view", &invert_view(&km)),
        Some("interact") => print_mode_bindings("interact", &invert_interact(&km)),
        Some("filter") => print_mode_bindings("filter", &invert_filter(&km)),
        Some(m) => {
            eprintln!("Unknown mode: {m}");
            eprintln!("Valid modes: normal, insert, view, interact, filter");
            return 1;
        }
        None => {
            print_mode_bindings("normal", &invert_normal(&km));
            println!();
            print_mode_bindings("insert", &invert_insert(&km));
            println!();
            print_mode_bindings("view", &invert_view(&km));
            println!();
            print_mode_bindings("interact", &invert_interact(&km));
            println!();
            print_mode_bindings("filter", &invert_filter(&km));
        }
    }
    0
}

fn print_mode_bindings(mode: &str, bindings: &[(String, Vec<String>)]) {
    println!("[{mode}]");
    for (action, keys) in bindings {
        let keys_str: Vec<String> = keys.iter().map(|k| format!("\"{k}\"")).collect();
        println!("{action} = [{}]", keys_str.join(", "));
    }
}

/// Invert a KeyCode->Action hashmap to Action->Vec<key_display_string>, sorted by action name.
fn invert_map<A: Eq + Copy>(
    map: &HashMap<KeyCode, A>,
    action_names: &[(A, &str)],
) -> Vec<(String, Vec<String>)> {
    let mut result = Vec::new();
    for &(action, name) in action_names {
        let mut keys: Vec<String> = map
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(k, _)| keymap::key_display(k))
            .collect();
        keys.sort();
        result.push((name.to_string(), keys));
    }
    result
}

fn invert_normal(km: &Keymap) -> Vec<(String, Vec<String>)> {
    invert_map(
        &km.normal,
        &[
            (NormalAction::Quit, "quit"),
            (NormalAction::Insert, "insert"),
            (NormalAction::SelectNext, "select_next"),
            (NormalAction::SelectPrev, "select_prev"),
            (NormalAction::ViewOutput, "view_output"),
            (NormalAction::Interact, "interact"),
            (NormalAction::IncreaseWorkers, "increase_workers"),
            (NormalAction::DecreaseWorkers, "decrease_workers"),
            (NormalAction::ToggleMode, "toggle_mode"),
            (NormalAction::Retry, "retry"),
            (NormalAction::Resume, "resume"),
            (NormalAction::MoveUp, "move_up"),
            (NormalAction::MoveDown, "move_down"),
            (NormalAction::Search, "search"),
            (NormalAction::HalfPageDown, "half_page_down"),
            (NormalAction::HalfPageUp, "half_page_up"),
            (NormalAction::GoToTop, "go_to_top"),
            (NormalAction::GoToBottom, "go_to_bottom"),
        ],
    )
}

fn invert_insert(km: &Keymap) -> Vec<(String, Vec<String>)> {
    invert_map(
        &km.insert,
        &[
            (InsertAction::Cancel, "cancel"),
            (InsertAction::Submit, "submit"),
            (InsertAction::AcceptSuggestion, "accept_suggestion"),
            (InsertAction::NextSuggestion, "next_suggestion"),
            (InsertAction::PrevSuggestion, "prev_suggestion"),
        ],
    )
}

fn invert_view(km: &Keymap) -> Vec<(String, Vec<String>)> {
    invert_map(
        &km.view,
        &[
            (ViewAction::Back, "back"),
            (ViewAction::ScrollDown, "scroll_down"),
            (ViewAction::ScrollUp, "scroll_up"),
            (ViewAction::Interact, "interact"),
            (ViewAction::ToggleAutoscroll, "toggle_autoscroll"),
            (ViewAction::KillWorker, "kill_worker"),
            (ViewAction::Export, "export"),
        ],
    )
}

fn invert_interact(km: &Keymap) -> Vec<(String, Vec<String>)> {
    invert_map(
        &km.interact,
        &[
            (InteractAction::Back, "back"),
            (InteractAction::Send, "send"),
        ],
    )
}

fn invert_filter(km: &Keymap) -> Vec<(String, Vec<String>)> {
    invert_map(
        &km.filter,
        &[
            (FilterAction::Confirm, "confirm"),
            (FilterAction::Cancel, "cancel"),
        ],
    )
}

fn keys_set(args: &[String]) -> i32 {
    if args.len() < 3 {
        eprintln!("Usage: clhorde keys set <mode> <action> <key1> [key2...]");
        return 1;
    }
    let mode = &args[0];
    let action = &args[1];
    let keys: Vec<String> = args[2..].to_vec();

    // Validate all keys
    for k in &keys {
        if keymap::parse_key(k).is_none() {
            eprintln!("Invalid key: {k}");
            return 1;
        }
    }

    let mut config = keymap::load_toml_config();
    if let Err(e) = set_toml_action(&mut config, mode, action, keys.clone()) {
        eprintln!("{e}");
        return 1;
    }

    if let Err(e) = keymap::save_toml_config(&config) {
        eprintln!("Failed to save config: {e}");
        return 1;
    }

    let keys_display: Vec<String> = keys.iter().map(|k| format!("\"{k}\"")).collect();
    println!("Set {mode}.{action} = [{}]", keys_display.join(", "));
    0
}

fn keys_reset(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: clhorde keys reset <mode> [action]");
        return 1;
    }
    let mode = &args[0];
    let action = args.get(1).map(|s| s.as_str());

    let mut config = keymap::load_toml_config();
    if let Err(e) = reset_toml_action(&mut config, mode, action) {
        eprintln!("{e}");
        return 1;
    }

    if let Err(e) = keymap::save_toml_config(&config) {
        eprintln!("Failed to save config: {e}");
        return 1;
    }

    match action {
        Some(a) => println!("Reset {mode}.{a} to default."),
        None => println!("Reset all {mode} bindings to defaults."),
    }
    0
}

// ── config subcommands ──

fn cmd_config(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("path") => config_path(),
        Some("edit") => config_edit(),
        Some("init") => config_init(args.get(1).map(|s| s.as_str()) == Some("--force")),
        _ => {
            eprintln!("Usage: clhorde config <path|edit|init>");
            eprintln!("  path          Print config file path");
            eprintln!("  edit          Open config in $EDITOR");
            eprintln!("  init [--force] Create config with defaults");
            1
        }
    }
}

fn config_path() -> i32 {
    match keymap::config_path() {
        Some(p) => {
            println!("{}", p.display());
            0
        }
        None => {
            eprintln!("Cannot determine config path.");
            1
        }
    }
}

fn config_edit() -> i32 {
    let path = match keymap::config_path() {
        Some(p) => p,
        None => {
            eprintln!("Cannot determine config path.");
            return 1;
        }
    };

    // Create file with defaults if it doesn't exist
    if !path.exists() {
        let config = Keymap::default_toml_config();
        if let Err(e) = keymap::save_toml_config(&config) {
            eprintln!("Failed to create config file: {e}");
            return 1;
        }
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    match std::process::Command::new(&editor)
        .arg(&path)
        .status()
    {
        Ok(status) => {
            if status.success() {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("Failed to open editor '{editor}': {e}");
            1
        }
    }
}

fn config_init(force: bool) -> i32 {
    let path = match keymap::config_path() {
        Some(p) => p,
        None => {
            eprintln!("Cannot determine config path.");
            return 1;
        }
    };

    if path.exists() && !force {
        eprintln!("Config file already exists: {}", path.display());
        eprintln!("Use --force to overwrite.");
        return 1;
    }

    let config = Keymap::default_toml_config();
    if let Err(e) = keymap::save_toml_config(&config) {
        eprintln!("Failed to write config: {e}");
        return 1;
    }
    println!("Created config: {}", path.display());
    0
}

// ── helpers ──

fn action_names_for_mode(mode: &str) -> Option<Vec<&'static str>> {
    match mode {
        "normal" => Some(vec![
            "quit",
            "insert",
            "select_next",
            "select_prev",
            "view_output",
            "interact",
            "increase_workers",
            "decrease_workers",
            "toggle_mode",
            "retry",
            "resume",
            "move_up",
            "move_down",
            "search",
            "half_page_down",
            "half_page_up",
            "go_to_top",
            "go_to_bottom",
        ]),
        "insert" => Some(vec![
            "cancel",
            "submit",
            "accept_suggestion",
            "next_suggestion",
            "prev_suggestion",
        ]),
        "view" => Some(vec![
            "back",
            "scroll_down",
            "scroll_up",
            "interact",
            "toggle_autoscroll",
            "kill_worker",
            "export",
        ]),
        "interact" => Some(vec!["back", "send"]),
        "filter" => Some(vec!["confirm", "cancel"]),
        _ => None,
    }
}

fn set_toml_action(
    config: &mut TomlConfig,
    mode: &str,
    action: &str,
    keys: Vec<String>,
) -> Result<(), String> {
    let valid = action_names_for_mode(mode)
        .ok_or_else(|| format!("Unknown mode: {mode}\nValid modes: normal, insert, view, interact, filter"))?;
    if !valid.contains(&action) {
        return Err(format!(
            "Unknown action '{action}' for mode '{mode}'.\nValid actions: {}",
            valid.join(", ")
        ));
    }

    let keys = Some(keys);

    match mode {
        "normal" => {
            let b = config.normal.get_or_insert_with(TomlNormalBindings::default);
            match action {
                "quit" => b.quit = keys,
                "insert" => b.insert = keys,
                "select_next" => b.select_next = keys,
                "select_prev" => b.select_prev = keys,
                "view_output" => b.view_output = keys,
                "interact" => b.interact = keys,
                "increase_workers" => b.increase_workers = keys,
                "decrease_workers" => b.decrease_workers = keys,
                "toggle_mode" => b.toggle_mode = keys,
                "retry" => b.retry = keys,
                "resume" => b.resume = keys,
                "move_up" => b.move_up = keys,
                "move_down" => b.move_down = keys,
                "search" => b.search = keys,
                "half_page_down" => b.half_page_down = keys,
                "half_page_up" => b.half_page_up = keys,
                "go_to_top" => b.go_to_top = keys,
                "go_to_bottom" => b.go_to_bottom = keys,
                _ => unreachable!(),
            }
        }
        "insert" => {
            let b = config.insert.get_or_insert_with(TomlInsertBindings::default);
            match action {
                "cancel" => b.cancel = keys,
                "submit" => b.submit = keys,
                "accept_suggestion" => b.accept_suggestion = keys,
                "next_suggestion" => b.next_suggestion = keys,
                "prev_suggestion" => b.prev_suggestion = keys,
                _ => unreachable!(),
            }
        }
        "view" => {
            let b = config.view.get_or_insert_with(TomlViewBindings::default);
            match action {
                "back" => b.back = keys,
                "scroll_down" => b.scroll_down = keys,
                "scroll_up" => b.scroll_up = keys,
                "interact" => b.interact = keys,
                "toggle_autoscroll" => b.toggle_autoscroll = keys,
                "kill_worker" => b.kill_worker = keys,
                "export" => b.export = keys,
                _ => unreachable!(),
            }
        }
        "interact" => {
            let b = config.interact.get_or_insert_with(TomlInteractBindings::default);
            match action {
                "back" => b.back = keys,
                "send" => b.send = keys,
                _ => unreachable!(),
            }
        }
        "filter" => {
            let b = config.filter.get_or_insert_with(TomlFilterBindings::default);
            match action {
                "confirm" => b.confirm = keys,
                "cancel" => b.cancel = keys,
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn reset_toml_action(
    config: &mut TomlConfig,
    mode: &str,
    action: Option<&str>,
) -> Result<(), String> {
    if let Some(action) = action {
        let valid = action_names_for_mode(mode)
            .ok_or_else(|| format!("Unknown mode: {mode}\nValid modes: normal, insert, view, interact, filter"))?;
        if !valid.contains(&action) {
            return Err(format!(
                "Unknown action '{action}' for mode '{mode}'.\nValid actions: {}",
                valid.join(", ")
            ));
        }
    } else {
        // Validate mode even when resetting all
        if action_names_for_mode(mode).is_none() {
            return Err(format!(
                "Unknown mode: {mode}\nValid modes: normal, insert, view, interact, filter"
            ));
        }
    }

    match (mode, action) {
        ("normal", None) => config.normal = None,
        ("normal", Some(a)) => {
            if let Some(b) = config.normal.as_mut() {
                match a {
                    "quit" => b.quit = None,
                    "insert" => b.insert = None,
                    "select_next" => b.select_next = None,
                    "select_prev" => b.select_prev = None,
                    "view_output" => b.view_output = None,
                    "interact" => b.interact = None,
                    "increase_workers" => b.increase_workers = None,
                    "decrease_workers" => b.decrease_workers = None,
                    "toggle_mode" => b.toggle_mode = None,
                    "retry" => b.retry = None,
                    "resume" => b.resume = None,
                    "move_up" => b.move_up = None,
                    "move_down" => b.move_down = None,
                    "search" => b.search = None,
                    "half_page_down" => b.half_page_down = None,
                    "half_page_up" => b.half_page_up = None,
                    "go_to_top" => b.go_to_top = None,
                    "go_to_bottom" => b.go_to_bottom = None,
                    _ => unreachable!(),
                }
            }
        }
        ("insert", None) => config.insert = None,
        ("insert", Some(a)) => {
            if let Some(b) = config.insert.as_mut() {
                match a {
                    "cancel" => b.cancel = None,
                    "submit" => b.submit = None,
                    "accept_suggestion" => b.accept_suggestion = None,
                    "next_suggestion" => b.next_suggestion = None,
                    "prev_suggestion" => b.prev_suggestion = None,
                    _ => unreachable!(),
                }
            }
        }
        ("view", None) => config.view = None,
        ("view", Some(a)) => {
            if let Some(b) = config.view.as_mut() {
                match a {
                    "back" => b.back = None,
                    "scroll_down" => b.scroll_down = None,
                    "scroll_up" => b.scroll_up = None,
                    "interact" => b.interact = None,
                    "toggle_autoscroll" => b.toggle_autoscroll = None,
                    "kill_worker" => b.kill_worker = None,
                    "export" => b.export = None,
                    _ => unreachable!(),
                }
            }
        }
        ("interact", None) => config.interact = None,
        ("interact", Some(a)) => {
            if let Some(b) = config.interact.as_mut() {
                match a {
                    "back" => b.back = None,
                    "send" => b.send = None,
                    _ => unreachable!(),
                }
            }
        }
        ("filter", None) => config.filter = None,
        ("filter", Some(a)) => {
            if let Some(b) = config.filter.as_mut() {
                match a {
                    "confirm" => b.confirm = None,
                    "cancel" => b.cancel = None,
                    _ => unreachable!(),
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_for_all_modes() {
        assert!(action_names_for_mode("normal").is_some());
        assert!(action_names_for_mode("insert").is_some());
        assert!(action_names_for_mode("view").is_some());
        assert!(action_names_for_mode("interact").is_some());
        assert!(action_names_for_mode("filter").is_some());
        assert!(action_names_for_mode("bogus").is_none());
    }

    #[test]
    fn set_toml_action_normal() {
        let mut config = TomlConfig::default();
        set_toml_action(&mut config, "normal", "quit", vec!["Q".into()]).unwrap();
        assert_eq!(
            config.normal.as_ref().unwrap().quit,
            Some(vec!["Q".into()])
        );
    }

    #[test]
    fn set_toml_action_invalid_mode() {
        let mut config = TomlConfig::default();
        let err = set_toml_action(&mut config, "bogus", "quit", vec!["q".into()]);
        assert!(err.is_err());
    }

    #[test]
    fn set_toml_action_invalid_action() {
        let mut config = TomlConfig::default();
        let err = set_toml_action(&mut config, "normal", "bogus", vec!["q".into()]);
        assert!(err.is_err());
    }

    #[test]
    fn set_toml_action_all_modes() {
        let mut config = TomlConfig::default();
        set_toml_action(&mut config, "insert", "cancel", vec!["Esc".into()]).unwrap();
        assert_eq!(
            config.insert.as_ref().unwrap().cancel,
            Some(vec!["Esc".into()])
        );

        set_toml_action(&mut config, "view", "back", vec!["Esc".into(), "q".into()]).unwrap();
        assert_eq!(
            config.view.as_ref().unwrap().back,
            Some(vec!["Esc".into(), "q".into()])
        );

        set_toml_action(&mut config, "interact", "send", vec!["Enter".into()]).unwrap();
        assert_eq!(
            config.interact.as_ref().unwrap().send,
            Some(vec!["Enter".into()])
        );

        set_toml_action(&mut config, "filter", "confirm", vec!["Enter".into()]).unwrap();
        assert_eq!(
            config.filter.as_ref().unwrap().confirm,
            Some(vec!["Enter".into()])
        );
    }

    #[test]
    fn reset_toml_action_single() {
        let mut config = TomlConfig::default();
        set_toml_action(&mut config, "normal", "quit", vec!["Q".into()]).unwrap();
        assert!(config.normal.as_ref().unwrap().quit.is_some());

        reset_toml_action(&mut config, "normal", Some("quit")).unwrap();
        assert!(config.normal.as_ref().unwrap().quit.is_none());
    }

    #[test]
    fn reset_toml_action_whole_mode() {
        let mut config = TomlConfig::default();
        set_toml_action(&mut config, "normal", "quit", vec!["Q".into()]).unwrap();
        assert!(config.normal.is_some());

        reset_toml_action(&mut config, "normal", None).unwrap();
        assert!(config.normal.is_none());
    }

    #[test]
    fn reset_toml_action_invalid_mode() {
        let mut config = TomlConfig::default();
        let err = reset_toml_action(&mut config, "bogus", None);
        assert!(err.is_err());
    }

    #[test]
    fn reset_toml_action_invalid_action() {
        let mut config = TomlConfig::default();
        let err = reset_toml_action(&mut config, "normal", Some("bogus"));
        assert!(err.is_err());
    }

    #[test]
    fn roundtrip_serialization() {
        let config = Keymap::default_toml_config();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: TomlConfig = toml::from_str(&serialized).unwrap();

        // Verify normal mode round-trips
        let orig_normal = config.normal.as_ref().unwrap();
        let deser_normal = deserialized.normal.as_ref().unwrap();
        assert_eq!(orig_normal.quit, deser_normal.quit);
        assert_eq!(orig_normal.insert, deser_normal.insert);
        assert_eq!(orig_normal.select_next, deser_normal.select_next);

        // Verify view mode round-trips
        let orig_view = config.view.as_ref().unwrap();
        let deser_view = deserialized.view.as_ref().unwrap();
        assert_eq!(orig_view.back, deser_view.back);
        assert_eq!(orig_view.scroll_down, deser_view.scroll_down);
    }

    #[test]
    fn run_returns_launch_tui_for_no_args() {
        // No subcommand -> LaunchTui
        assert!(matches!(run(&["clhorde".into()]), CliAction::LaunchTui(opts) if opts.prompts.is_empty()));
    }

    #[test]
    fn run_dispatches_subcommands() {
        // These should return Exit (handled), even if the subcommand fails
        assert!(matches!(run(&["clhorde".into(), "qp".into()]), CliAction::Exit(_)));
        assert!(matches!(run(&["clhorde".into(), "keys".into()]), CliAction::Exit(_)));
        assert!(matches!(run(&["clhorde".into(), "config".into()]), CliAction::Exit(_)));
        assert!(matches!(run(&["clhorde".into(), "store".into()]), CliAction::Exit(_)));
    }

    #[test]
    fn run_dispatches_help() {
        assert!(matches!(run(&["clhorde".into(), "help".into()]), CliAction::Exit(0)));
        assert!(matches!(run(&["clhorde".into(), "--help".into()]), CliAction::Exit(0)));
        assert!(matches!(run(&["clhorde".into(), "-h".into()]), CliAction::Exit(0)));
    }

    #[test]
    fn run_unknown_command_launches_tui() {
        assert!(matches!(run(&["clhorde".into(), "unknown".into()]), CliAction::LaunchTui(opts) if opts.prompts.is_empty()));
    }

    #[test]
    fn prompt_from_files_no_args() {
        assert!(matches!(cmd_prompt_from_files(&[]), CliAction::Exit(1)));
    }

    #[test]
    fn prompt_from_files_reads_files() {
        let dir = std::env::temp_dir().join(format!("clhorde-pff-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();

        let f1 = dir.join("p1.txt");
        let f2 = dir.join("p2.txt");
        fs::write(&f1, "prompt one").unwrap();
        fs::write(&f2, "prompt two").unwrap();

        let args = vec![
            f1.to_string_lossy().to_string(),
            f2.to_string_lossy().to_string(),
        ];
        match cmd_prompt_from_files(&args) {
            CliAction::LaunchTui(opts) => {
                assert_eq!(opts.prompts.len(), 2);
                assert_eq!(opts.prompts[0], "prompt one");
                assert_eq!(opts.prompts[1], "prompt two");
                assert!(opts.worktree);
                assert!(opts.run_path.is_none());
            }
            _ => panic!("Expected LaunchTui"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_from_files_comma_separated() {
        let dir = std::env::temp_dir().join(format!("clhorde-pff-comma-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();

        let f1 = dir.join("a.txt");
        let f2 = dir.join("b.txt");
        fs::write(&f1, "alpha").unwrap();
        fs::write(&f2, "beta").unwrap();

        let arg = format!("{},{}", f1.display(), f2.display());
        match cmd_prompt_from_files(&[arg]) {
            CliAction::LaunchTui(opts) => {
                assert_eq!(opts.prompts.len(), 2);
                assert_eq!(opts.prompts[0], "alpha");
                assert_eq!(opts.prompts[1], "beta");
                assert!(opts.worktree);
            }
            _ => panic!("Expected LaunchTui"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_from_files_skips_empty_and_missing() {
        let dir = std::env::temp_dir().join(format!("clhorde-pff-skip-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();

        let f1 = dir.join("good.txt");
        let f2 = dir.join("empty.txt");
        fs::write(&f1, "valid prompt").unwrap();
        fs::write(&f2, "   ").unwrap(); // whitespace-only = empty after trim

        let args = vec![
            f1.to_string_lossy().to_string(),
            f2.to_string_lossy().to_string(),
            "/tmp/nonexistent-clhorde-test-file.txt".to_string(),
        ];
        match cmd_prompt_from_files(&args) {
            CliAction::LaunchTui(opts) => {
                assert_eq!(opts.prompts.len(), 1);
                assert_eq!(opts.prompts[0], "valid prompt");
            }
            _ => panic!("Expected LaunchTui"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_from_files_all_invalid_exits() {
        let args = vec![
            "/tmp/nonexistent-clhorde-1.txt".to_string(),
            "/tmp/nonexistent-clhorde-2.txt".to_string(),
        ];
        assert!(matches!(cmd_prompt_from_files(&args), CliAction::Exit(1)));
    }

    #[test]
    fn prompt_from_files_run_path() {
        let dir = std::env::temp_dir().join(format!("clhorde-pff-rp-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();

        let f1 = dir.join("task.txt");
        fs::write(&f1, "do something").unwrap();

        let args = vec![
            "--run-path".to_string(),
            dir.to_string_lossy().to_string(),
            f1.to_string_lossy().to_string(),
        ];
        match cmd_prompt_from_files(&args) {
            CliAction::LaunchTui(opts) => {
                assert_eq!(opts.prompts.len(), 1);
                assert_eq!(opts.prompts[0], "do something");
                assert!(opts.worktree);
                assert_eq!(opts.run_path.as_deref(), Some(dir.to_string_lossy().as_ref()));
            }
            _ => panic!("Expected LaunchTui"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_from_files_run_path_nonexistent() {
        let args = vec![
            "--run-path".to_string(),
            "/tmp/nonexistent-clhorde-dir-xyz".to_string(),
            "some_file.txt".to_string(),
        ];
        assert!(matches!(cmd_prompt_from_files(&args), CliAction::Exit(1)));
    }

    #[test]
    fn prompt_from_files_run_path_missing_value() {
        let args = vec!["--run-path".to_string()];
        assert!(matches!(cmd_prompt_from_files(&args), CliAction::Exit(1)));
    }

    // ── store subcommand tests ──

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
        // Uses real dir — may or may not be empty, but should not crash
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

        // We can't call store_drop directly with a custom dir, so test the
        // underlying persistence logic that store_drop uses.
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

        // Drop only "completed"
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

        // Keep only "completed"
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
