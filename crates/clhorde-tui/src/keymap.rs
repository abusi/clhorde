use std::collections::HashMap;

use crossterm::event::KeyCode;

// Re-export core keymap items used by other TUI modules (cli.rs, app.rs)
pub use clhorde_core::keymap::{
    TomlConfig, TomlNormalBindings, TomlInsertBindings, TomlViewBindings,
    TomlInteractBindings, TomlFilterBindings,
    parse_key, key_display, config_path, load_settings, load_toml_config, save_toml_config,
    apply_bindings, keys_for_action, format_keys,
};

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
    Resume,
    MoveUp,
    MoveDown,
    Search,
    HalfPageDown,
    HalfPageUp,
    GoToTop,
    GoToBottom,
    ShrinkList,
    GrowList,
    ShowHelp,
    ToggleSelect,
    SelectAllVisible,
    VisualSelect,
    DeleteSelected,
    KillSelected,
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
    ToggleSplit,
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
        normal.insert(KeyCode::Char('R'), NormalAction::Resume);
        normal.insert(KeyCode::Char('J'), NormalAction::MoveDown);
        normal.insert(KeyCode::Char('K'), NormalAction::MoveUp);
        normal.insert(KeyCode::Char('/'), NormalAction::Search);
        normal.insert(KeyCode::Char('G'), NormalAction::GoToBottom);
        normal.insert(KeyCode::Char('h'), NormalAction::ShrinkList);
        normal.insert(KeyCode::Char('l'), NormalAction::GrowList);
        normal.insert(KeyCode::Char('?'), NormalAction::ShowHelp);
        normal.insert(KeyCode::Char(' '), NormalAction::ToggleSelect);
        normal.insert(KeyCode::Char('V'), NormalAction::SelectAllVisible);
        normal.insert(KeyCode::Char('v'), NormalAction::VisualSelect);
        normal.insert(KeyCode::Char('d'), NormalAction::DeleteSelected);
        normal.insert(KeyCode::Char('x'), NormalAction::KillSelected);

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
        view.insert(KeyCode::Char('t'), ViewAction::ToggleSplit);

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

impl Keymap {
    pub fn load() -> Self {
        let path = match config_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };

        let config: TomlConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };

        Self::from_toml(config)
    }

    pub(crate) fn from_toml(config: TomlConfig) -> Self {
        let mut keymap = Self::default();

        if let Some(normal) = config.normal {
            apply_bindings(&mut keymap.normal, NormalAction::Quit, normal.quit);
            apply_bindings(&mut keymap.normal, NormalAction::Insert, normal.insert);
            apply_bindings(&mut keymap.normal, NormalAction::SelectNext, normal.select_next);
            apply_bindings(&mut keymap.normal, NormalAction::SelectPrev, normal.select_prev);
            apply_bindings(&mut keymap.normal, NormalAction::ViewOutput, normal.view_output);
            apply_bindings(&mut keymap.normal, NormalAction::Interact, normal.interact);
            apply_bindings(&mut keymap.normal, NormalAction::IncreaseWorkers, normal.increase_workers);
            apply_bindings(&mut keymap.normal, NormalAction::DecreaseWorkers, normal.decrease_workers);
            apply_bindings(&mut keymap.normal, NormalAction::ToggleMode, normal.toggle_mode);
            apply_bindings(&mut keymap.normal, NormalAction::Retry, normal.retry);
            apply_bindings(&mut keymap.normal, NormalAction::Resume, normal.resume);
            apply_bindings(&mut keymap.normal, NormalAction::MoveUp, normal.move_up);
            apply_bindings(&mut keymap.normal, NormalAction::MoveDown, normal.move_down);
            apply_bindings(&mut keymap.normal, NormalAction::Search, normal.search);
            apply_bindings(&mut keymap.normal, NormalAction::HalfPageDown, normal.half_page_down);
            apply_bindings(&mut keymap.normal, NormalAction::HalfPageUp, normal.half_page_up);
            apply_bindings(&mut keymap.normal, NormalAction::GoToTop, normal.go_to_top);
            apply_bindings(&mut keymap.normal, NormalAction::GoToBottom, normal.go_to_bottom);
            apply_bindings(&mut keymap.normal, NormalAction::ShrinkList, normal.shrink_list);
            apply_bindings(&mut keymap.normal, NormalAction::GrowList, normal.grow_list);
            apply_bindings(&mut keymap.normal, NormalAction::ShowHelp, normal.show_help);
            apply_bindings(&mut keymap.normal, NormalAction::ToggleSelect, normal.toggle_select);
            apply_bindings(&mut keymap.normal, NormalAction::SelectAllVisible, normal.select_all_visible);
            apply_bindings(&mut keymap.normal, NormalAction::VisualSelect, normal.visual_select);
            apply_bindings(&mut keymap.normal, NormalAction::DeleteSelected, normal.delete_selected);
            apply_bindings(&mut keymap.normal, NormalAction::KillSelected, normal.kill_selected);
        }

        if let Some(insert) = config.insert {
            apply_bindings(&mut keymap.insert, InsertAction::Cancel, insert.cancel);
            apply_bindings(&mut keymap.insert, InsertAction::Submit, insert.submit);
            apply_bindings(&mut keymap.insert, InsertAction::AcceptSuggestion, insert.accept_suggestion);
            apply_bindings(&mut keymap.insert, InsertAction::NextSuggestion, insert.next_suggestion);
            apply_bindings(&mut keymap.insert, InsertAction::PrevSuggestion, insert.prev_suggestion);
        }

        if let Some(view) = config.view {
            apply_bindings(&mut keymap.view, ViewAction::Back, view.back);
            apply_bindings(&mut keymap.view, ViewAction::ScrollDown, view.scroll_down);
            apply_bindings(&mut keymap.view, ViewAction::ScrollUp, view.scroll_up);
            apply_bindings(&mut keymap.view, ViewAction::Interact, view.interact);
            apply_bindings(&mut keymap.view, ViewAction::ToggleAutoscroll, view.toggle_autoscroll);
            apply_bindings(&mut keymap.view, ViewAction::KillWorker, view.kill_worker);
            apply_bindings(&mut keymap.view, ViewAction::Export, view.export);
            apply_bindings(&mut keymap.view, ViewAction::ToggleSplit, view.toggle_split);
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

    /// Return a TomlConfig with all defaults populated (for `config init`).
    pub(crate) fn default_toml_config() -> TomlConfig {
        let km = Self::default();

        fn keys_to_strings<A: PartialEq>(map: &HashMap<KeyCode, A>, action: A) -> Vec<String> {
            let mut keys: Vec<_> = map
                .iter()
                .filter(|(_, a)| **a == action)
                .map(|(k, _)| key_display(k))
                .collect();
            keys.sort();
            keys
        }

        TomlConfig {
            settings: None,
            normal: Some(TomlNormalBindings {
                quit: Some(keys_to_strings(&km.normal, NormalAction::Quit)),
                insert: Some(keys_to_strings(&km.normal, NormalAction::Insert)),
                select_next: Some(keys_to_strings(&km.normal, NormalAction::SelectNext)),
                select_prev: Some(keys_to_strings(&km.normal, NormalAction::SelectPrev)),
                view_output: Some(keys_to_strings(&km.normal, NormalAction::ViewOutput)),
                interact: Some(keys_to_strings(&km.normal, NormalAction::Interact)),
                increase_workers: Some(keys_to_strings(&km.normal, NormalAction::IncreaseWorkers)),
                decrease_workers: Some(keys_to_strings(&km.normal, NormalAction::DecreaseWorkers)),
                toggle_mode: Some(keys_to_strings(&km.normal, NormalAction::ToggleMode)),
                retry: Some(keys_to_strings(&km.normal, NormalAction::Retry)),
                resume: Some(keys_to_strings(&km.normal, NormalAction::Resume)),
                move_up: Some(keys_to_strings(&km.normal, NormalAction::MoveUp)),
                move_down: Some(keys_to_strings(&km.normal, NormalAction::MoveDown)),
                search: Some(keys_to_strings(&km.normal, NormalAction::Search)),
                half_page_down: Some(keys_to_strings(&km.normal, NormalAction::HalfPageDown)),
                half_page_up: Some(keys_to_strings(&km.normal, NormalAction::HalfPageUp)),
                go_to_top: Some(keys_to_strings(&km.normal, NormalAction::GoToTop)),
                go_to_bottom: Some(keys_to_strings(&km.normal, NormalAction::GoToBottom)),
                shrink_list: Some(keys_to_strings(&km.normal, NormalAction::ShrinkList)),
                grow_list: Some(keys_to_strings(&km.normal, NormalAction::GrowList)),
                show_help: Some(keys_to_strings(&km.normal, NormalAction::ShowHelp)),
                toggle_select: Some(keys_to_strings(&km.normal, NormalAction::ToggleSelect)),
                select_all_visible: Some(keys_to_strings(&km.normal, NormalAction::SelectAllVisible)),
                visual_select: Some(keys_to_strings(&km.normal, NormalAction::VisualSelect)),
                delete_selected: Some(keys_to_strings(&km.normal, NormalAction::DeleteSelected)),
                kill_selected: Some(keys_to_strings(&km.normal, NormalAction::KillSelected)),
            }),
            insert: Some(TomlInsertBindings {
                cancel: Some(keys_to_strings(&km.insert, InsertAction::Cancel)),
                submit: Some(keys_to_strings(&km.insert, InsertAction::Submit)),
                accept_suggestion: Some(keys_to_strings(&km.insert, InsertAction::AcceptSuggestion)),
                next_suggestion: Some(keys_to_strings(&km.insert, InsertAction::NextSuggestion)),
                prev_suggestion: Some(keys_to_strings(&km.insert, InsertAction::PrevSuggestion)),
            }),
            view: Some(TomlViewBindings {
                back: Some(keys_to_strings(&km.view, ViewAction::Back)),
                scroll_down: Some(keys_to_strings(&km.view, ViewAction::ScrollDown)),
                scroll_up: Some(keys_to_strings(&km.view, ViewAction::ScrollUp)),
                interact: Some(keys_to_strings(&km.view, ViewAction::Interact)),
                toggle_autoscroll: Some(keys_to_strings(&km.view, ViewAction::ToggleAutoscroll)),
                kill_worker: Some(keys_to_strings(&km.view, ViewAction::KillWorker)),
                export: Some(keys_to_strings(&km.view, ViewAction::Export)),
                toggle_split: Some(keys_to_strings(&km.view, ViewAction::ToggleSplit)),
            }),
            interact: Some(TomlInteractBindings {
                back: Some(keys_to_strings(&km.interact, InteractAction::Back)),
                send: Some(keys_to_strings(&km.interact, InteractAction::Send)),
            }),
            filter: Some(TomlFilterBindings {
                confirm: Some(keys_to_strings(&km.filter, FilterAction::Confirm)),
                cancel: Some(keys_to_strings(&km.filter, FilterAction::Cancel)),
            }),
            quick_prompts: None,
        }
    }

    pub fn normal_help(&self) -> Vec<(String, &'static str)> {
        let entries: &[(NormalAction, &str)] = &[
            (NormalAction::Insert, "insert"),
            (NormalAction::Quit, "quit"),
            (NormalAction::SelectNext, "next"),
            (NormalAction::SelectPrev, "prev"),
            (NormalAction::HalfPageDown, "½pg dn"),
            (NormalAction::HalfPageUp, "½pg up"),
            (NormalAction::GoToTop, "top"),
            (NormalAction::GoToBottom, "bottom"),
            (NormalAction::ViewOutput, "view"),
            (NormalAction::Interact, "interact"),
            (NormalAction::Retry, "retry"),
            (NormalAction::Resume, "resume"),
            (NormalAction::Search, "search"),
            (NormalAction::MoveUp, "move up"),
            (NormalAction::MoveDown, "move down"),
            (NormalAction::IncreaseWorkers, "more wkrs"),
            (NormalAction::DecreaseWorkers, "less wkrs"),
            (NormalAction::ToggleMode, "mode"),
            (NormalAction::ShrinkList, "shrink"),
            (NormalAction::GrowList, "grow"),
            (NormalAction::ShowHelp, "help"),
            (NormalAction::ToggleSelect, "select"),
            (NormalAction::SelectAllVisible, "sel all"),
            (NormalAction::VisualSelect, "visual"),
            (NormalAction::DeleteSelected, "delete"),
            (NormalAction::KillSelected, "kill"),
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
            (ViewAction::ToggleSplit, "split"),
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

    pub fn normal_key_hint(&self, action: NormalAction) -> String {
        let keys = keys_for_action(&self.normal, action);
        if keys.is_empty() {
            "?".to_string()
        } else {
            key_display(&keys[0])
        }
    }

    pub fn view_key_hint(&self, action: ViewAction) -> String {
        let keys = keys_for_action(&self.view, action);
        if keys.is_empty() {
            "?".to_string()
        } else {
            key_display(&keys[0])
        }
    }

    pub fn quick_prompt_help(&self) -> Vec<(String, String)> {
        let mut entries: Vec<_> = self
            .quick_prompts
            .iter()
            .map(|(kc, msg)| (key_display(kc), msg.clone()))
            .collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries
    }

    fn build_help<A: PartialEq + Copy>(
        &self,
        map: &HashMap<KeyCode, A>,
        entries: &[(A, &'static str)],
    ) -> Vec<(String, &'static str)> {
        let mut result = Vec::new();
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
    fn from_toml_partial_override() {
        let toml_str = r#"
[normal]
quit = ["Q"]
"#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let km = Keymap::from_toml(config);
        assert_eq!(km.normal.get(&KeyCode::Char('q')), None);
        assert_eq!(km.normal.get(&KeyCode::Char('Q')), Some(&NormalAction::Quit));
        assert_eq!(km.normal.get(&KeyCode::Char('i')), Some(&NormalAction::Insert));
    }

    #[test]
    fn from_toml_quick_prompts() {
        let toml_str = r#"
[quick_prompts]
g = "let's go"
c = "continue"
"#;
        let config: TomlConfig = toml::from_str(toml_str).unwrap();
        let km = Keymap::from_toml(config);
        assert_eq!(km.quick_prompts.len(), 2);
        assert_eq!(km.quick_prompts.get(&KeyCode::Char('g')), Some(&"let's go".to_string()));
    }

    #[test]
    fn normal_help_contains_expected_entries() {
        let km = Keymap::default();
        let help = km.normal_help();
        let labels: Vec<&str> = help.iter().map(|(_, l)| *l).collect();
        assert!(labels.contains(&"insert"));
        assert!(labels.contains(&"quit"));
        assert!(labels.contains(&"view"));
    }
}
