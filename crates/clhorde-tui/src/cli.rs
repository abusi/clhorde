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
        "prompt-from-files" => cmd_prompt_from_files(&args[2..]),
        _ => CliAction::LaunchTui(LaunchOptions { prompts: vec![], worktree: false, run_path: None }),
    }
}

fn cmd_help() -> i32 {
    println!("clhorde {}", env!("CARGO_PKG_VERSION"));
    println!("A TUI for orchestrating multiple Claude Code CLI instances in parallel.");
    println!();
    println!("Usage: clhorde [options]");
    println!();
    println!("Options:");
    println!("  (none)              Launch the TUI");
    println!("  prompt-from-files [--run-path <path>] <files...>");
    println!("                      Load prompts from files and launch TUI");
    println!("                      Each prompt runs in its own git worktree");
    println!("                      --run-path sets the working directory for all prompts");
    println!("  --help, -h          Show this help");
    println!();
    println!("For config management, use clhorde-cli:");
    println!("  clhorde-cli store   Manage persisted prompts");
    println!("  clhorde-cli qp      Manage quick prompts");
    println!("  clhorde-cli keys    Manage keybindings");
    println!("  clhorde-cli config  Manage config file");
    0
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn run_returns_launch_tui_for_no_args() {
        assert!(matches!(run(&["clhorde".into()]), CliAction::LaunchTui(opts) if opts.prompts.is_empty()));
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
        fs::write(&f2, "   ").unwrap();

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
}
