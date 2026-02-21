mod commands;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = run(&args);
    std::process::exit(code);
}

fn run(args: &[String]) -> i32 {
    match args.get(1).map(|s| s.as_str()) {
        Some("help") | Some("--help") | Some("-h") => cmd_help(),
        Some("store") => commands::store::cmd_store(&args[2..]),
        Some("qp") => commands::qp::cmd_qp(&args[2..]),
        Some("keys") => commands::keys::cmd_keys(&args[2..]),
        Some("config") => commands::config::cmd_config(&args[2..]),
        _ => {
            cmd_help();
            1
        }
    }
}

fn cmd_help() -> i32 {
    println!("clhorde-cli {}", env!("CARGO_PKG_VERSION"));
    println!("CLI utilities for clhorde configuration and prompt management.");
    println!();
    println!("Usage: clhorde-cli <command> [options]");
    println!();
    println!("Commands:");
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
    println!();
    println!("Modes: normal, insert, view, interact, filter");
    println!();
    println!("Filters for drop/keep: all, completed, failed, pending");
    println!();
    println!("Examples:");
    println!("  clhorde-cli store list");
    println!("  clhorde-cli store drop all");
    println!("  clhorde-cli store drop failed");
    println!("  clhorde-cli store keep completed");
    println!("  clhorde-cli qp add g \"let's go\"");
    println!("  clhorde-cli keys set normal quit Q");
    println!("  clhorde-cli keys list normal");
    println!("  clhorde-cli config init");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_help_variants() {
        assert_eq!(run(&["clhorde-cli".into(), "help".into()]), 0);
        assert_eq!(run(&["clhorde-cli".into(), "--help".into()]), 0);
        assert_eq!(run(&["clhorde-cli".into(), "-h".into()]), 0);
    }

    #[test]
    fn run_dispatches_subcommands() {
        // These should return non-zero (no sub-args) but not panic
        assert_eq!(run(&["clhorde-cli".into(), "store".into()]), 1);
        assert_eq!(run(&["clhorde-cli".into(), "keys".into()]), 1);
        assert_eq!(run(&["clhorde-cli".into(), "config".into()]), 1);
        assert_eq!(run(&["clhorde-cli".into(), "qp".into()]), 1);
    }

    #[test]
    fn run_unknown_shows_help() {
        assert_eq!(run(&["clhorde-cli".into(), "bogus".into()]), 1);
    }

    #[test]
    fn run_no_args_shows_help() {
        assert_eq!(run(&["clhorde-cli".into()]), 1);
    }
}
