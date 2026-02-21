//! Keymap configuration: TOML deserialization types, key parsing/display,
//! config file I/O, and settings loading.
//!
//! The runtime `Keymap` struct and action enums live in clhorde-tui.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use crossterm::event::KeyCode;
use serde::{Deserialize, Serialize};

// ── TOML deserialization types ──

#[derive(Deserialize, Serialize, Default)]
pub struct TomlConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<TomlSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normal: Option<TomlNormalBindings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert: Option<TomlInsertBindings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<TomlViewBindings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interact: Option<TomlInteractBindings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<TomlFilterBindings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_prompts: Option<HashMap<String, String>>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct TomlSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_saved_prompts: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_cleanup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_ratio: Option<u8>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct TomlNormalBindings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quit: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insert: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select_next: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select_prev: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_output: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interact: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub increase_workers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decrease_workers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle_mode: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_up: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_down: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub half_page_down: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub half_page_up: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub go_to_top: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub go_to_bottom: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shrink_list: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grow_list: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_help: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle_select: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select_all_visible: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visual_select: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_selected: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill_selected: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct TomlInsertBindings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_suggestion: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_suggestion: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_suggestion: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct TomlViewBindings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_down: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_up: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interact: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle_autoscroll: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill_worker: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle_split: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct TomlInteractBindings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Default)]
pub struct TomlFilterBindings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel: Option<Vec<String>>,
}

// ── Key parsing and display ──

pub fn parse_key(s: &str) -> Option<KeyCode> {
    match s {
        "Enter" => Some(KeyCode::Enter),
        "Esc" => Some(KeyCode::Esc),
        "Tab" => Some(KeyCode::Tab),
        "Backspace" => Some(KeyCode::Backspace),
        "Up" => Some(KeyCode::Up),
        "Down" => Some(KeyCode::Down),
        "Left" => Some(KeyCode::Left),
        "Right" => Some(KeyCode::Right),
        "Space" => Some(KeyCode::Char(' ')),
        s if s.len() == 1 => s.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

pub fn key_display(kc: &KeyCode) -> String {
    match kc {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        _ => "?".to_string(),
    }
}

// ── Config file I/O ──

pub fn config_path() -> Option<PathBuf> {
    let config_dir = env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(config_dir.join("clhorde").join("keymap.toml"))
}

/// Load settings from the config file.
pub fn load_settings() -> TomlSettings {
    let config = load_toml_config();
    config.settings.unwrap_or_default()
}

/// Load the raw TOML config (not the resolved Keymap). Returns Default if file missing.
pub fn load_toml_config() -> TomlConfig {
    let path = match config_path() {
        Some(p) => p,
        None => return TomlConfig::default(),
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return TomlConfig::default(),
    };
    toml::from_str(&content).unwrap_or_default()
}

/// Save a TomlConfig to the config file, creating parent dirs as needed.
pub fn save_toml_config(config: &TomlConfig) -> io::Result<()> {
    let path = config_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine config path"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)
        .map_err(io::Error::other)?;
    fs::write(&path, content)
}

/// Remove all existing bindings for `action`, then insert new ones from `keys`.
/// If `keys` is None, keep defaults.
pub fn apply_bindings<A: PartialEq + Copy>(
    map: &mut HashMap<KeyCode, A>,
    action: A,
    keys: Option<Vec<String>>,
) {
    let keys = match keys {
        Some(k) => k,
        None => return,
    };

    // Remove old bindings for this action
    map.retain(|_, v| *v != action);

    // Insert new bindings
    for key_str in &keys {
        if let Some(kc) = parse_key(key_str) {
            map.insert(kc, action);
        }
    }
}

/// Collect all keys bound to a given action, sorted for display consistency.
pub fn keys_for_action<A: PartialEq>(map: &HashMap<KeyCode, A>, action: A) -> Vec<KeyCode> {
    let mut keys: Vec<KeyCode> = map
        .iter()
        .filter(|(_, a)| **a == action)
        .map(|(k, _)| *k)
        .collect();
    keys.sort_by_key(key_display);
    keys
}

/// Format a list of keycodes as a display string like "j/k" or "Esc/q".
pub fn format_keys(keys: &[KeyCode]) -> String {
    keys.iter()
        .map(key_display)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_key ──

    #[test]
    fn parse_key_special_names() {
        assert_eq!(parse_key("Enter"), Some(KeyCode::Enter));
        assert_eq!(parse_key("Esc"), Some(KeyCode::Esc));
        assert_eq!(parse_key("Tab"), Some(KeyCode::Tab));
        assert_eq!(parse_key("Backspace"), Some(KeyCode::Backspace));
        assert_eq!(parse_key("Up"), Some(KeyCode::Up));
        assert_eq!(parse_key("Down"), Some(KeyCode::Down));
        assert_eq!(parse_key("Left"), Some(KeyCode::Left));
        assert_eq!(parse_key("Right"), Some(KeyCode::Right));
        assert_eq!(parse_key("Space"), Some(KeyCode::Char(' ')));
    }

    #[test]
    fn parse_key_single_chars() {
        assert_eq!(parse_key("q"), Some(KeyCode::Char('q')));
        assert_eq!(parse_key("i"), Some(KeyCode::Char('i')));
        assert_eq!(parse_key("+"), Some(KeyCode::Char('+')));
        assert_eq!(parse_key("/"), Some(KeyCode::Char('/')));
        assert_eq!(parse_key("K"), Some(KeyCode::Char('K')));
    }

    #[test]
    fn parse_key_invalid() {
        assert_eq!(parse_key(""), None);
        assert_eq!(parse_key("Unknown"), None);
        assert_eq!(parse_key("Ctrl+A"), None);
        assert_eq!(parse_key("ab"), None);
    }

    // ── key_display roundtrip ──

    #[test]
    fn key_display_roundtrip() {
        let names = [
            "Enter", "Esc", "Tab", "Backspace", "Up", "Down", "Left", "Right", "Space",
            "q", "i", "+", "/",
        ];
        for name in names {
            let kc = parse_key(name).unwrap();
            assert_eq!(key_display(&kc), name, "roundtrip failed for {name}");
        }
    }

    // ── apply_bindings ──

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestAction { A, B }

    #[test]
    fn apply_bindings_removes_old_keys() {
        let mut map = HashMap::new();
        map.insert(KeyCode::Char('q'), TestAction::A);
        map.insert(KeyCode::Char('i'), TestAction::B);

        apply_bindings(&mut map, TestAction::A, Some(vec!["x".to_string()]));

        assert_eq!(map.get(&KeyCode::Char('q')), None);
        assert_eq!(map.get(&KeyCode::Char('x')), Some(&TestAction::A));
        assert_eq!(map.get(&KeyCode::Char('i')), Some(&TestAction::B));
    }

    #[test]
    fn apply_bindings_none_keeps_defaults() {
        let mut map = HashMap::new();
        map.insert(KeyCode::Char('q'), TestAction::A);

        apply_bindings(&mut map, TestAction::A, None);

        assert_eq!(map.get(&KeyCode::Char('q')), Some(&TestAction::A));
    }
}
