use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use crossterm::event::KeyCode;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalAction {
    Quit,
    Insert,
    SelectNext,
    SelectPrev,
    ViewOutput,
    Interact,
    IncreaseWorkers,
    DecreaseWorkers,
    ToggleMode,
    Retry,
    MoveUp,
    MoveDown,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertAction {
    Cancel,
    Submit,
    AcceptSuggestion,
    NextSuggestion,
    PrevSuggestion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewAction {
    Back,
    ScrollDown,
    ScrollUp,
    Interact,
    ToggleAutoscroll,
    KillWorker,
    Export,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractAction {
    Back,
    Send,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    Confirm,
    Cancel,
}

pub struct Keymap {
    pub normal: HashMap<KeyCode, NormalAction>,
    pub insert: HashMap<KeyCode, InsertAction>,
    pub view: HashMap<KeyCode, ViewAction>,
    pub interact: HashMap<KeyCode, InteractAction>,
    pub filter: HashMap<KeyCode, FilterAction>,
    pub quick_prompts: HashMap<KeyCode, String>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut normal = HashMap::new();
        normal.insert(KeyCode::Char('q'), NormalAction::Quit);
        normal.insert(KeyCode::Char('i'), NormalAction::Insert);
        normal.insert(KeyCode::Char('j'), NormalAction::SelectNext);
        normal.insert(KeyCode::Down, NormalAction::SelectNext);
        normal.insert(KeyCode::Char('k'), NormalAction::SelectPrev);
        normal.insert(KeyCode::Up, NormalAction::SelectPrev);
        normal.insert(KeyCode::Enter, NormalAction::ViewOutput);
        normal.insert(KeyCode::Char('s'), NormalAction::Interact);
        normal.insert(KeyCode::Char('+'), NormalAction::IncreaseWorkers);
        normal.insert(KeyCode::Char('='), NormalAction::IncreaseWorkers);
        normal.insert(KeyCode::Char('-'), NormalAction::DecreaseWorkers);
        normal.insert(KeyCode::Char('m'), NormalAction::ToggleMode);
        normal.insert(KeyCode::Char('r'), NormalAction::Retry);
        normal.insert(KeyCode::Char('J'), NormalAction::MoveDown);
        normal.insert(KeyCode::Char('K'), NormalAction::MoveUp);
        normal.insert(KeyCode::Char('/'), NormalAction::Search);

        let mut insert = HashMap::new();
        insert.insert(KeyCode::Esc, InsertAction::Cancel);
        insert.insert(KeyCode::Enter, InsertAction::Submit);
        insert.insert(KeyCode::Tab, InsertAction::AcceptSuggestion);
        insert.insert(KeyCode::Down, InsertAction::NextSuggestion);
        insert.insert(KeyCode::Up, InsertAction::PrevSuggestion);

        let mut view = HashMap::new();
        view.insert(KeyCode::Esc, ViewAction::Back);
        view.insert(KeyCode::Char('q'), ViewAction::Back);
        view.insert(KeyCode::Char('j'), ViewAction::ScrollDown);
        view.insert(KeyCode::Down, ViewAction::ScrollDown);
        view.insert(KeyCode::Char('k'), ViewAction::ScrollUp);
        view.insert(KeyCode::Up, ViewAction::ScrollUp);
        view.insert(KeyCode::Char('s'), ViewAction::Interact);
        view.insert(KeyCode::Char('f'), ViewAction::ToggleAutoscroll);
        view.insert(KeyCode::Char('x'), ViewAction::KillWorker);
        view.insert(KeyCode::Char('w'), ViewAction::Export);

        let mut interact = HashMap::new();
        interact.insert(KeyCode::Esc, InteractAction::Back);
        interact.insert(KeyCode::Enter, InteractAction::Send);

        let mut filter = HashMap::new();
        filter.insert(KeyCode::Esc, FilterAction::Cancel);
        filter.insert(KeyCode::Enter, FilterAction::Confirm);

        Self {
            normal,
            insert,
            view,
            interact,
            filter,
            quick_prompts: HashMap::new(),
        }
    }
}

// TOML deserialization types

#[derive(Deserialize, Default)]
struct TomlConfig {
    normal: Option<TomlNormalBindings>,
    insert: Option<TomlInsertBindings>,
    view: Option<TomlViewBindings>,
    interact: Option<TomlInteractBindings>,
    filter: Option<TomlFilterBindings>,
    quick_prompts: Option<HashMap<String, String>>,
}

#[derive(Deserialize, Default)]
struct TomlNormalBindings {
    quit: Option<Vec<String>>,
    insert: Option<Vec<String>>,
    select_next: Option<Vec<String>>,
    select_prev: Option<Vec<String>>,
    view_output: Option<Vec<String>>,
    interact: Option<Vec<String>>,
    increase_workers: Option<Vec<String>>,
    decrease_workers: Option<Vec<String>>,
    toggle_mode: Option<Vec<String>>,
    retry: Option<Vec<String>>,
    move_up: Option<Vec<String>>,
    move_down: Option<Vec<String>>,
    search: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct TomlInsertBindings {
    cancel: Option<Vec<String>>,
    submit: Option<Vec<String>>,
    accept_suggestion: Option<Vec<String>>,
    next_suggestion: Option<Vec<String>>,
    prev_suggestion: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct TomlViewBindings {
    back: Option<Vec<String>>,
    scroll_down: Option<Vec<String>>,
    scroll_up: Option<Vec<String>>,
    interact: Option<Vec<String>>,
    toggle_autoscroll: Option<Vec<String>>,
    kill_worker: Option<Vec<String>>,
    export: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct TomlInteractBindings {
    back: Option<Vec<String>>,
    send: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
struct TomlFilterBindings {
    confirm: Option<Vec<String>>,
    cancel: Option<Vec<String>>,
}

fn parse_key(s: &str) -> Option<KeyCode> {
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

fn config_path() -> Option<PathBuf> {
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

impl Keymap {
    pub fn load() -> Self {
        let path = match config_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };

        let config: TomlConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };

        Self::from_toml(config)
    }

    fn from_toml(config: TomlConfig) -> Self {
        let mut keymap = Self::default();

        if let Some(normal) = config.normal {
            apply_bindings(&mut keymap.normal, NormalAction::Quit, normal.quit);
            apply_bindings(&mut keymap.normal, NormalAction::Insert, normal.insert);
            apply_bindings(&mut keymap.normal, NormalAction::SelectNext, normal.select_next);
            apply_bindings(&mut keymap.normal, NormalAction::SelectPrev, normal.select_prev);
            apply_bindings(&mut keymap.normal, NormalAction::ViewOutput, normal.view_output);
            apply_bindings(&mut keymap.normal, NormalAction::Interact, normal.interact);
            apply_bindings(
                &mut keymap.normal,
                NormalAction::IncreaseWorkers,
                normal.increase_workers,
            );
            apply_bindings(
                &mut keymap.normal,
                NormalAction::DecreaseWorkers,
                normal.decrease_workers,
            );
            apply_bindings(&mut keymap.normal, NormalAction::ToggleMode, normal.toggle_mode);
            apply_bindings(&mut keymap.normal, NormalAction::Retry, normal.retry);
            apply_bindings(&mut keymap.normal, NormalAction::MoveUp, normal.move_up);
            apply_bindings(&mut keymap.normal, NormalAction::MoveDown, normal.move_down);
            apply_bindings(&mut keymap.normal, NormalAction::Search, normal.search);
        }

        if let Some(insert) = config.insert {
            apply_bindings(&mut keymap.insert, InsertAction::Cancel, insert.cancel);
            apply_bindings(&mut keymap.insert, InsertAction::Submit, insert.submit);
            apply_bindings(
                &mut keymap.insert,
                InsertAction::AcceptSuggestion,
                insert.accept_suggestion,
            );
            apply_bindings(
                &mut keymap.insert,
                InsertAction::NextSuggestion,
                insert.next_suggestion,
            );
            apply_bindings(
                &mut keymap.insert,
                InsertAction::PrevSuggestion,
                insert.prev_suggestion,
            );
        }

        if let Some(view) = config.view {
            apply_bindings(&mut keymap.view, ViewAction::Back, view.back);
            apply_bindings(&mut keymap.view, ViewAction::ScrollDown, view.scroll_down);
            apply_bindings(&mut keymap.view, ViewAction::ScrollUp, view.scroll_up);
            apply_bindings(&mut keymap.view, ViewAction::Interact, view.interact);
            apply_bindings(
                &mut keymap.view,
                ViewAction::ToggleAutoscroll,
                view.toggle_autoscroll,
            );
            apply_bindings(&mut keymap.view, ViewAction::KillWorker, view.kill_worker);
            apply_bindings(&mut keymap.view, ViewAction::Export, view.export);
        }

        if let Some(interact) = config.interact {
            apply_bindings(&mut keymap.interact, InteractAction::Back, interact.back);
            apply_bindings(&mut keymap.interact, InteractAction::Send, interact.send);
        }

        if let Some(filter) = config.filter {
            apply_bindings(&mut keymap.filter, FilterAction::Confirm, filter.confirm);
            apply_bindings(&mut keymap.filter, FilterAction::Cancel, filter.cancel);
        }

        if let Some(qp) = config.quick_prompts {
            for (key_str, message) in qp {
                if let Some(kc) = parse_key(&key_str) {
                    keymap.quick_prompts.insert(kc, message);
                }
            }
        }

        keymap
    }
}

/// Remove all existing bindings for `action`, then insert new ones from `keys`.
/// If `keys` is None, keep defaults.
fn apply_bindings<A: PartialEq + Copy>(
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

// Help bar generation

fn key_display(kc: &KeyCode) -> String {
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

/// Collect all keys bound to a given action, sorted for display consistency.
fn keys_for_action<A: PartialEq>(map: &HashMap<KeyCode, A>, action: A) -> Vec<KeyCode> {
    let mut keys: Vec<KeyCode> = map
        .iter()
        .filter(|(_, a)| **a == action)
        .map(|(k, _)| *k)
        .collect();
    keys.sort_by_key(key_display);
    keys
}

/// Format a list of keycodes as a display string like "j/k" or "Esc/q".
fn format_keys(keys: &[KeyCode]) -> String {
    keys.iter()
        .map(key_display)
        .collect::<Vec<_>>()
        .join("/")
}

impl Keymap {
    pub fn normal_help(&self) -> Vec<(String, &'static str)> {
        let entries: &[(NormalAction, &str)] = &[
            (NormalAction::Insert, "insert"),
            (NormalAction::Quit, "quit"),
            (NormalAction::SelectNext, "next"),
            (NormalAction::SelectPrev, "prev"),
            (NormalAction::ViewOutput, "view"),
            (NormalAction::Interact, "interact"),
            (NormalAction::Retry, "retry"),
            (NormalAction::Search, "search"),
            (NormalAction::MoveUp, "move up"),
            (NormalAction::MoveDown, "move down"),
            (NormalAction::IncreaseWorkers, "more wkrs"),
            (NormalAction::DecreaseWorkers, "less wkrs"),
            (NormalAction::ToggleMode, "mode"),
        ];
        self.build_help(&self.normal, entries)
    }

    pub fn insert_help(&self) -> Vec<(String, &'static str)> {
        let entries: &[(InsertAction, &str)] = &[
            (InsertAction::Submit, "submit"),
            (InsertAction::Cancel, "cancel"),
            (InsertAction::AcceptSuggestion, "complete dir"),
        ];
        self.build_help(&self.insert, entries)
    }

    pub fn view_help(&self) -> Vec<(String, &'static str)> {
        let entries: &[(ViewAction, &str)] = &[
            (ViewAction::Back, "back"),
            (ViewAction::ScrollDown, "down"),
            (ViewAction::ScrollUp, "up"),
            (ViewAction::Interact, "interact"),
            (ViewAction::ToggleAutoscroll, "auto-scroll"),
            (ViewAction::KillWorker, "kill"),
            (ViewAction::Export, "export"),
        ];
        self.build_help(&self.view, entries)
    }

    pub fn interact_help(&self) -> Vec<(String, &'static str)> {
        let entries: &[(InteractAction, &str)] = &[
            (InteractAction::Send, "send"),
            (InteractAction::Back, "back"),
        ];
        self.build_help(&self.interact, entries)
    }

    pub fn filter_help(&self) -> Vec<(String, &'static str)> {
        let entries: &[(FilterAction, &str)] = &[
            (FilterAction::Confirm, "apply"),
            (FilterAction::Cancel, "cancel"),
        ];
        self.build_help(&self.filter, entries)
    }

    /// Look up the first key bound to a NormalAction for display in hints.
    pub fn normal_key_hint(&self, action: NormalAction) -> String {
        let keys = keys_for_action(&self.normal, action);
        if keys.is_empty() {
            "?".to_string()
        } else {
            key_display(&keys[0])
        }
    }

    /// Look up the first key bound to a ViewAction for display in hints.
    pub fn view_key_hint(&self, action: ViewAction) -> String {
        let keys = keys_for_action(&self.view, action);
        if keys.is_empty() {
            "?".to_string()
        } else {
            key_display(&keys[0])
        }
    }

    fn build_help<A: PartialEq + Copy>(
        &self,
        map: &HashMap<KeyCode, A>,
        entries: &[(A, &'static str)],
    ) -> Vec<(String, &'static str)> {
        // Group actions that share adjacent display slots and merge their keys
        let mut result = Vec::new();
        // Track which actions we've already emitted (to merge next/prev style pairs)
        let mut seen_actions: Vec<A> = Vec::new();

        for &(action, label) in entries {
            if seen_actions.contains(&action) {
                continue;
            }
            seen_actions.push(action);
            let keys = keys_for_action(map, action);
            if keys.is_empty() {
                continue;
            }
            result.push((format_keys(&keys), label));
        }
        result
    }
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

    // ── Keymap::default ──

    #[test]
    fn default_normal_bindings() {
        let km = Keymap::default();
        assert_eq!(km.normal.get(&KeyCode::Char('q')), Some(&NormalAction::Quit));
        assert_eq!(km.normal.get(&KeyCode::Char('i')), Some(&NormalAction::Insert));
        assert_eq!(km.normal.get(&KeyCode::Char('j')), Some(&NormalAction::SelectNext));
        assert_eq!(km.normal.get(&KeyCode::Down), Some(&NormalAction::SelectNext));
        assert_eq!(km.normal.get(&KeyCode::Enter), Some(&NormalAction::ViewOutput));
        assert_eq!(km.normal.get(&KeyCode::Char('m')), Some(&NormalAction::ToggleMode));
        assert_eq!(km.normal.get(&KeyCode::Char('r')), Some(&NormalAction::Retry));
        assert_eq!(km.normal.get(&KeyCode::Char('/')), Some(&NormalAction::Search));
    }

    #[test]
    fn default_view_bindings() {
        let km = Keymap::default();
        assert_eq!(km.view.get(&KeyCode::Esc), Some(&ViewAction::Back));
        assert_eq!(km.view.get(&KeyCode::Char('q')), Some(&ViewAction::Back));
        assert_eq!(km.view.get(&KeyCode::Char('f')), Some(&ViewAction::ToggleAutoscroll));
        assert_eq!(km.view.get(&KeyCode::Char('x')), Some(&ViewAction::KillWorker));
        assert_eq!(km.view.get(&KeyCode::Char('w')), Some(&ViewAction::Export));
    }

    #[test]
    fn default_insert_bindings() {
        let km = Keymap::default();
        assert_eq!(km.insert.get(&KeyCode::Esc), Some(&InsertAction::Cancel));
        assert_eq!(km.insert.get(&KeyCode::Enter), Some(&InsertAction::Submit));
        assert_eq!(km.insert.get(&KeyCode::Tab), Some(&InsertAction::AcceptSuggestion));
    }

    #[test]
    fn default_interact_bindings() {
        let km = Keymap::default();
        assert_eq!(km.interact.get(&KeyCode::Esc), Some(&InteractAction::Back));
        assert_eq!(km.interact.get(&KeyCode::Enter), Some(&InteractAction::Send));
    }

    #[test]
    fn default_filter_bindings() {
        let km = Keymap::default();
        assert_eq!(km.filter.get(&KeyCode::Esc), Some(&FilterAction::Cancel));
        assert_eq!(km.filter.get(&KeyCode::Enter), Some(&FilterAction::Confirm));
    }

    // ── from_toml partial override ──

    #[test]
    fn from_toml_partial_override() {
        let toml_str = r#"
[normal]
quit = ["Q"]
"#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let km = Keymap::from_toml(config);

        // Old quit key removed, new one works
        assert_eq!(km.normal.get(&KeyCode::Char('q')), None);
        assert_eq!(km.normal.get(&KeyCode::Char('Q')), Some(&NormalAction::Quit));

        // Other bindings unchanged
        assert_eq!(km.normal.get(&KeyCode::Char('i')), Some(&NormalAction::Insert));
        assert_eq!(km.normal.get(&KeyCode::Char('j')), Some(&NormalAction::SelectNext));
        assert_eq!(km.normal.get(&KeyCode::Enter), Some(&NormalAction::ViewOutput));
    }

    #[test]
    fn from_toml_empty_config() {
        let config: TomlConfig = toml::from_str("").unwrap();
        let km = Keymap::from_toml(config);
        let default = Keymap::default();

        // Spot-check that empty config produces same result as default
        assert_eq!(km.normal.len(), default.normal.len());
        assert_eq!(km.insert.len(), default.insert.len());
        assert_eq!(km.view.len(), default.view.len());
        assert_eq!(km.interact.len(), default.interact.len());
        assert_eq!(km.filter.len(), default.filter.len());

        for (key, action) in &default.normal {
            assert_eq!(km.normal.get(key), Some(action));
        }
    }

    // ── apply_bindings ──

    #[test]
    fn apply_bindings_removes_old_keys() {
        let mut map = HashMap::new();
        map.insert(KeyCode::Char('q'), NormalAction::Quit);
        map.insert(KeyCode::Char('i'), NormalAction::Insert);

        apply_bindings(&mut map, NormalAction::Quit, Some(vec!["x".to_string()]));

        assert_eq!(map.get(&KeyCode::Char('q')), None);
        assert_eq!(map.get(&KeyCode::Char('x')), Some(&NormalAction::Quit));
        // Unrelated binding untouched
        assert_eq!(map.get(&KeyCode::Char('i')), Some(&NormalAction::Insert));
    }

    #[test]
    fn apply_bindings_none_keeps_defaults() {
        let mut map = HashMap::new();
        map.insert(KeyCode::Char('q'), NormalAction::Quit);

        apply_bindings(&mut map, NormalAction::Quit, None);

        assert_eq!(map.get(&KeyCode::Char('q')), Some(&NormalAction::Quit));
    }

    #[test]
    fn apply_bindings_multiple_keys() {
        let mut map = HashMap::new();
        map.insert(KeyCode::Char('q'), NormalAction::Quit);

        apply_bindings(
            &mut map,
            NormalAction::Quit,
            Some(vec!["x".to_string(), "X".to_string()]),
        );

        assert_eq!(map.get(&KeyCode::Char('q')), None);
        assert_eq!(map.get(&KeyCode::Char('x')), Some(&NormalAction::Quit));
        assert_eq!(map.get(&KeyCode::Char('X')), Some(&NormalAction::Quit));
    }

    // ── help bar generation ──

    #[test]
    fn normal_help_contains_expected_entries() {
        let km = Keymap::default();
        let help = km.normal_help();
        let labels: Vec<&str> = help.iter().map(|(_, l)| *l).collect();

        assert!(labels.contains(&"insert"), "missing 'insert' in help");
        assert!(labels.contains(&"quit"), "missing 'quit' in help");
        assert!(labels.contains(&"view"), "missing 'view' in help");
        assert!(labels.contains(&"mode"), "missing 'mode' in help");
        assert!(labels.contains(&"retry"), "missing 'retry' in help");
    }

    #[test]
    fn help_skips_unbound_actions() {
        let mut km = Keymap::default();
        // Remove all quit bindings
        km.normal.retain(|_, v| *v != NormalAction::Quit);
        let help = km.normal_help();
        let labels: Vec<&str> = help.iter().map(|(_, l)| *l).collect();
        assert!(!labels.contains(&"quit"));
    }

    #[test]
    fn key_hint_returns_question_mark_for_unbound() {
        let mut km = Keymap::default();
        km.normal.retain(|_, v| *v != NormalAction::Insert);
        assert_eq!(km.normal_key_hint(NormalAction::Insert), "?");
    }

    #[test]
    fn key_hint_returns_key_for_bound() {
        let km = Keymap::default();
        assert_eq!(km.normal_key_hint(NormalAction::Insert), "i");
        assert_eq!(km.view_key_hint(ViewAction::ToggleAutoscroll), "f");
    }

    // ── quick_prompts ──

    #[test]
    fn default_has_no_quick_prompts() {
        let km = Keymap::default();
        assert!(km.quick_prompts.is_empty());
    }

    #[test]
    fn from_toml_parses_quick_prompts() {
        let toml_str = r#"
[quick_prompts]
g = "let's go"
c = "continue"
"#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let km = Keymap::from_toml(config);

        assert_eq!(km.quick_prompts.len(), 2);
        assert_eq!(
            km.quick_prompts.get(&KeyCode::Char('g')),
            Some(&"let's go".to_string())
        );
        assert_eq!(
            km.quick_prompts.get(&KeyCode::Char('c')),
            Some(&"continue".to_string())
        );
    }

    #[test]
    fn from_toml_quick_prompts_ignores_invalid_keys() {
        let toml_str = r#"
[quick_prompts]
g = "go"
InvalidKey = "nope"
"#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let km = Keymap::from_toml(config);

        assert_eq!(km.quick_prompts.len(), 1);
        assert_eq!(
            km.quick_prompts.get(&KeyCode::Char('g')),
            Some(&"go".to_string())
        );
    }
}
