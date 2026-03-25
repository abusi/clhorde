use std::collections::HashMap;

use crossterm::event::KeyCode;

use clhorde_core::keymap::{
    self, FilterAction, InsertAction, InteractAction, Keymap, NormalAction, TomlConfig,
    TomlFilterBindings, TomlInsertBindings, TomlInteractBindings, TomlNormalBindings,
    TomlViewBindings, ViewAction,
};

pub fn cmd_keys(args: &[String]) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("list") => keys_list(args.get(1).map(|s| s.as_str())),
        Some("set") => keys_set(&args[1..]),
        Some("reset") => keys_reset(&args[1..]),
        _ => {
            eprintln!("Usage: clhorde-cli keys <list|set|reset>");
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
        eprintln!("Usage: clhorde-cli keys set <mode> <action> <key1> [key2...]");
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
        eprintln!("Usage: clhorde-cli keys reset <mode> [action]");
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
    let valid = action_names_for_mode(mode).ok_or_else(|| {
        format!("Unknown mode: {mode}\nValid modes: normal, insert, view, interact, filter")
    })?;
    if !valid.contains(&action) {
        return Err(format!(
            "Unknown action '{action}' for mode '{mode}'.\nValid actions: {}",
            valid.join(", ")
        ));
    }

    let keys = Some(keys);

    match mode {
        "normal" => {
            let b = config
                .normal
                .get_or_insert_with(TomlNormalBindings::default);
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
            let b = config
                .insert
                .get_or_insert_with(TomlInsertBindings::default);
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
            let b = config
                .interact
                .get_or_insert_with(TomlInteractBindings::default);
            match action {
                "back" => b.back = keys,
                "send" => b.send = keys,
                _ => unreachable!(),
            }
        }
        "filter" => {
            let b = config
                .filter
                .get_or_insert_with(TomlFilterBindings::default);
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
        let valid = action_names_for_mode(mode).ok_or_else(|| {
            format!("Unknown mode: {mode}\nValid modes: normal, insert, view, interact, filter")
        })?;
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
        assert_eq!(config.normal.as_ref().unwrap().quit, Some(vec!["Q".into()]));
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

        let orig_normal = config.normal.as_ref().unwrap();
        let deser_normal = deserialized.normal.as_ref().unwrap();
        assert_eq!(orig_normal.quit, deser_normal.quit);
        assert_eq!(orig_normal.insert, deser_normal.insert);
        assert_eq!(orig_normal.select_next, deser_normal.select_next);

        let orig_view = config.view.as_ref().unwrap();
        let deser_view = deserialized.view.as_ref().unwrap();
        assert_eq!(orig_view.back, deser_view.back);
        assert_eq!(orig_view.scroll_down, deser_view.scroll_down);
    }
}
