use std::collections::HashMap;

use clhorde_core::keymap;

pub fn cmd_qp(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => qp_list(),
        Some("add") => qp_add(&args[1..]),
        Some("remove") => qp_remove(&args[1..]),
        _ => {
            eprintln!("Usage: clhorde-cli qp <list|add|remove>");
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
        eprintln!("Usage: clhorde-cli qp add <key> <message...>");
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
        eprintln!("Usage: clhorde-cli qp remove <key>");
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
