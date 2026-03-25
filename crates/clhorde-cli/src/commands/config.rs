use clhorde_core::keymap::{self, Keymap};

pub fn cmd_config(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("path") => config_path(),
        Some("edit") => config_edit(),
        Some("init") => config_init(args.get(1).map(|s| s.as_str()) == Some("--force")),
        _ => {
            eprintln!("Usage: clhorde-cli config <path|edit|init>");
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
    match std::process::Command::new(&editor).arg(&path).status() {
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
