use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::editor::TextBuffer;
use crate::keymap::{
    FilterAction, InsertAction, InteractAction, Keymap, NormalAction, ViewAction,
};
use clhorde_core::prompt::PromptStatus;
use crate::orchestrator::{MoveResult, MsgEffect, Orchestrator};
use crate::pty_worker;
use crate::worker::WorkerMessage;

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Insert,
    ViewOutput,
    Interact,
    /// Raw keystroke forwarding to PTY worker.
    PtyInteract,
    Filter,
}

pub struct App {
    pub orch: Orchestrator,
    pub mode: AppMode,
    pub list_state: ListState,
    pub input: TextBuffer,
    /// Flag: Ctrl+E pressed in insert mode, main.rs should open $EDITOR.
    pub open_external_editor: bool,
    pub scroll_offset: u16,
    pub should_quit: bool,
    pub interact_input: String,
    pub auto_scroll: bool,
    pub suggestions: Vec<String>,
    pub suggestion_index: usize,
    /// Tick counter incremented every 100ms, used for pulsing animations.
    pub tick: u64,
    pub keymap: Keymap,
    /// Transient status message shown in the output viewer title.
    pub status_message: Option<(String, Instant)>,
    /// Whether quit confirmation dialog is showing.
    pub confirm_quit: bool,
    /// Active filter text (None = no filter).
    pub filter_text: Option<String>,
    /// Input buffer for filter mode.
    pub filter_input: String,
    /// Cached list of prompt indices matching the current filter.
    pub filtered_indices: Vec<usize>,
    /// Command history (most recent last).
    pub history: Vec<String>,
    /// Current position in history navigation (None = not navigating).
    pub history_index: Option<usize>,
    /// Stashed input text when entering history navigation.
    pub history_stash: String,
    /// Prompt templates loaded from config.
    pub templates: std::collections::HashMap<String, String>,
    /// Template suggestion names matching current input.
    pub template_suggestions: Vec<String>,
    /// Selected template suggestion index.
    pub template_suggestion_index: usize,
    /// Whether the quick prompts popup is visible (toggled by Ctrl+P in view mode).
    pub show_quick_prompts_popup: bool,
    /// Size of the output panel (cols, rows) from last render.
    pub output_panel_size: Option<(u16, u16)>,
    /// Last PTY size sent to workers (for change detection).
    pub last_pty_size: Option<(u16, u16)>,
    /// Whether the next submitted prompt should use a git worktree.
    pub worktree_pending: bool,
    /// Height of the prompt list panel (set during rendering).
    pub list_height: u16,
    /// Whether `g` was pressed once (waiting for second `g` for gg → go to top).
    pub pending_g: bool,
    /// Panel split ratio (percentage for list panel, 10–90).
    pub list_ratio: u16,
    /// Whether the list panel is collapsed (output maximized).
    pub list_collapsed: bool,
    /// Wall-clock time when the session started.
    pub session_start: Instant,
    /// Whether the full-screen help overlay is visible.
    pub show_help_overlay: bool,
    /// Scroll offset for the help overlay content.
    pub help_scroll: u16,
    /// Recently moved prompt: (prompt_id, Instant) for flash highlight.
    pub recently_moved: Option<(usize, Instant)>,
    /// Set of selected prompt IDs for batch operations.
    pub selected_ids: HashSet<usize>,
    /// Whether visual select mode is active (j/k extends selection).
    pub visual_select_active: bool,
    /// Whether batch delete confirmation dialog is showing.
    pub confirm_batch_delete: bool,
}

impl App {
    pub fn new() -> Self {
        let orch = Orchestrator::new();
        let mut list_state = ListState::default();
        if !orch.prompts.is_empty() {
            list_state.select(Some(0));
        }
        let filtered_indices: Vec<usize> = (0..orch.prompts.len()).collect();
        let templates = clhorde_core::config::load_templates();
        let history = clhorde_core::config::load_history();
        let settings = crate::keymap::load_settings();
        let list_ratio = (settings.list_ratio.unwrap_or(40) as u16).clamp(10, 90);

        Self {
            orch,
            mode: AppMode::Normal,
            list_state,
            input: TextBuffer::new(),
            open_external_editor: false,
            scroll_offset: 0,
            should_quit: false,
            interact_input: String::new(),
            auto_scroll: true,
            suggestions: Vec::new(),
            suggestion_index: 0,
            tick: 0,
            keymap: Keymap::load(),
            status_message: None,
            confirm_quit: false,
            filter_text: None,
            filter_input: String::new(),
            filtered_indices,
            history,
            history_index: None,
            history_stash: String::new(),
            templates,
            template_suggestions: Vec::new(),
            template_suggestion_index: 0,
            show_quick_prompts_popup: false,
            output_panel_size: None,
            last_pty_size: None,
            worktree_pending: false,
            list_height: 0,
            pending_g: false,
            list_ratio,
            list_collapsed: false,
            session_start: Instant::now(),
            show_help_overlay: false,
            help_scroll: 0,
            recently_moved: None,
            selected_ids: HashSet::new(),
            visual_select_active: false,
            confirm_batch_delete: false,
        }
    }

    // ── Delegated counts ──

    pub fn pending_count(&self) -> usize {
        self.orch.pending_count()
    }

    pub fn completed_count(&self) -> usize {
        self.orch.completed_count()
    }

    // ── Add prompt (delegates + rebuilds filter) ──

    pub fn add_prompt(&mut self, text: String, cwd: Option<String>, worktree: bool, tags: Vec<String>) {
        self.orch.add_prompt(text, cwd, worktree, tags);
        self.rebuild_filter();
        if self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }
    }

    fn parse_cwd_prefix(input: &str) -> (Option<String>, String) {
        if let Some((prefix, rest)) = input.split_once(": ") {
            let prefix = prefix.trim();
            if !prefix.is_empty() && Path::new(prefix).is_dir() {
                return (Some(prefix.to_string()), rest.trim().to_string());
            }
        }
        (None, input.to_string())
    }

    fn update_suggestions(&mut self) {
        self.suggestions.clear();
        self.suggestion_index = 0;

        let input_str = self.input.first_line();
        if input_str.contains(": ") {
            return;
        }

        let input = input_str.trim();
        if input.is_empty() {
            return;
        }

        let path = Path::new(input);

        let (parent, prefix) = if input.ends_with('/') && path.is_dir() {
            (path.to_path_buf(), String::new())
        } else {
            match (path.parent(), path.file_name()) {
                (Some(p), Some(name)) => {
                    let parent = if p.as_os_str().is_empty() {
                        Path::new(".").to_path_buf()
                    } else {
                        p.to_path_buf()
                    };
                    (parent, name.to_string_lossy().to_string())
                }
                _ => return,
            }
        };

        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        let mut matches: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(&prefix) {
                    Some(e.path().to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();

        matches.sort();
        if matches.len() > 10 {
            matches.truncate(10);
        }

        self.suggestions = matches;
    }

    fn accept_suggestion(&mut self) {
        if let Some(path) = self.suggestions.get(self.suggestion_index).cloned() {
            self.input.set(&format!("{path}/"));
            self.suggestions.clear();
            self.suggestion_index = 0;
            self.update_suggestions();
        }
    }

    pub fn selected_prompt(&self) -> Option<&clhorde_core::prompt::Prompt> {
        self.list_state
            .selected()
            .and_then(|i| self.orch.prompts.get(i))
    }

    /// Mark the currently selected prompt as seen if it's finished.
    fn mark_selected_seen(&mut self) {
        if let Some(idx) = self.list_state.selected() {
            if let Some(prompt) = self.orch.prompts.get_mut(idx) {
                if prompt.status == PromptStatus::Completed || prompt.status == PromptStatus::Failed
                {
                    prompt.seen = true;
                }
            }
        }
    }

    /// Clear expired status messages (older than 3 seconds) and stale move highlights.
    pub fn clear_expired_status(&mut self) {
        if let Some((_, created)) = &self.status_message {
            if created.elapsed().as_secs() >= 3 {
                self.status_message = None;
            }
        }
        if let Some((_, t)) = &self.recently_moved {
            if t.elapsed().as_millis() >= 300 {
                self.recently_moved = None;
            }
        }
    }

    // ── apply_message wrapper ──

    pub fn apply_message(&mut self, msg: WorkerMessage) {
        let effect = self.orch.apply_message(msg);
        self.handle_msg_effect(effect);
    }

    pub fn handle_msg_effect(&mut self, effect: MsgEffect) {
        if let MsgEffect::PromptFinished { prompt_id } = effect {
            if self.mode == AppMode::PtyInteract {
                if let Some(p) = self.selected_prompt() {
                    if p.id == prompt_id {
                        self.mode = AppMode::ViewOutput;
                    }
                }
            }
        }
    }

    // ── resize PTY wrapper (updates last_pty_size) ──

    pub fn resize_pty_workers(&mut self, cols: u16, rows: u16) {
        self.orch.resize_pty_workers(cols, rows);
        self.last_pty_size = Some((cols, rows));
    }

    // ── Key handling ──

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Help overlay intercepts all keys
        if self.show_help_overlay {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.show_help_overlay = false;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                _ => {}
            }
            return;
        }

        // Quit confirmation intercepts all keys
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.should_quit = true,
                _ => self.confirm_quit = false,
            }
            return;
        }

        // Batch delete confirmation intercepts all keys
        if self.confirm_batch_delete {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.execute_batch_delete();
                    self.confirm_batch_delete = false;
                }
                _ => self.confirm_batch_delete = false,
            }
            return;
        }

        match self.mode {
            AppMode::Normal => self.handle_normal_key(key),
            AppMode::Insert => self.handle_insert_key(key),
            AppMode::ViewOutput => self.handle_view_key(key),
            AppMode::Interact => self.handle_interact_key(key),
            AppMode::PtyInteract => self.handle_pty_interact_key(key),
            AppMode::Filter => self.handle_filter_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // Handle gg sequence: second g completes go-to-top
        if self.pending_g {
            self.pending_g = false;
            if key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::NONE {
                self.select_first();
                self.mark_selected_seen();
                return;
            }
        }

        // Ctrl+D → half page down
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            let before = self.list_state.selected();
            self.select_half_page_down();
            if self.visual_select_active {
                self.extend_selection_range(before, self.list_state.selected());
            }
            self.mark_selected_seen();
            return;
        }
        // Ctrl+U → half page up
        if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) {
            let before = self.list_state.selected();
            self.select_half_page_up();
            if self.visual_select_active {
                self.extend_selection_range(before, self.list_state.selected());
            }
            self.mark_selected_seen();
            return;
        }

        // Esc clears selection in Normal mode
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            if !self.selected_ids.is_empty() || self.visual_select_active {
                self.clear_selection();
            }
            return;
        }

        // g starts the gg sequence
        if key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::NONE
            && !self.keymap.normal.contains_key(&KeyCode::Char('g'))
        {
            self.pending_g = true;
            return;
        }

        let Some(action) = self.keymap.normal.get(&key.code) else {
            return;
        };
        match action {
            NormalAction::Quit => {
                let has_active = self.orch.prompts.iter().any(|p| {
                    p.status == PromptStatus::Running || p.status == PromptStatus::Idle
                });
                if has_active {
                    self.confirm_quit = true;
                } else {
                    self.should_quit = true;
                }
            }
            NormalAction::Insert => {
                self.mode = AppMode::Insert;
                self.input.clear();
                self.open_external_editor = false;
                self.history_index = None;
                self.history_stash.clear();
                self.template_suggestions.clear();
                self.template_suggestion_index = 0;
                self.worktree_pending = false;
            }
            NormalAction::SelectNext => {
                self.select_next();
                if self.visual_select_active {
                    if let Some(idx) = self.list_state.selected() {
                        if let Some(prompt) = self.orch.prompts.get(idx) {
                            self.selected_ids.insert(prompt.id);
                        }
                    }
                }
                self.mark_selected_seen();
            }
            NormalAction::SelectPrev => {
                self.select_prev();
                if self.visual_select_active {
                    if let Some(idx) = self.list_state.selected() {
                        if let Some(prompt) = self.orch.prompts.get(idx) {
                            self.selected_ids.insert(prompt.id);
                        }
                    }
                }
                self.mark_selected_seen();
            }
            NormalAction::ViewOutput => {
                if let Some(idx) = self.list_state.selected() {
                    if idx < self.orch.prompts.len() {
                        self.orch.prompts[idx].seen = true;
                        self.scroll_offset = 0;
                        self.mode = AppMode::ViewOutput;
                        self.list_collapsed = true;
                    }
                }
            }
            NormalAction::Interact => {
                let target_mode = {
                    let prompt_info = self.selected_prompt().map(|p| (p.id, p.status.clone()));
                    prompt_info.and_then(|(id, status)| {
                        if status == PromptStatus::Running || status == PromptStatus::Idle {
                            if self.orch.pty_handles.contains_key(&id) {
                                Some(AppMode::PtyInteract)
                            } else {
                                Some(AppMode::Interact)
                            }
                        } else {
                            None
                        }
                    })
                };
                if let Some(mode) = target_mode {
                    self.scroll_offset = 0;
                    if mode == AppMode::Interact {
                        self.interact_input.clear();
                    }
                    self.mode = mode;
                    self.list_collapsed = true;
                }
            }
            NormalAction::IncreaseWorkers => {
                self.orch.max_workers = (self.orch.max_workers + 1).min(20);
            }
            NormalAction::DecreaseWorkers => {
                self.orch.max_workers = self.orch.max_workers.saturating_sub(1).max(1);
            }
            NormalAction::ToggleMode => {
                if !self.selected_ids.is_empty() {
                    self.batch_toggle_mode();
                } else {
                    self.orch.default_mode = self.orch.default_mode.toggle();
                }
            }
            NormalAction::Retry => {
                if !self.selected_ids.is_empty() {
                    self.batch_retry();
                } else {
                    self.retry_selected();
                }
            }
            NormalAction::Resume => {
                self.resume_selected();
            }
            NormalAction::MoveUp => {
                self.move_selected_up();
            }
            NormalAction::MoveDown => {
                self.move_selected_down();
            }
            NormalAction::Search => {
                self.filter_input.clear();
                self.mode = AppMode::Filter;
            }
            NormalAction::HalfPageDown => {
                let before = self.list_state.selected();
                self.select_half_page_down();
                if self.visual_select_active {
                    self.extend_selection_range(before, self.list_state.selected());
                }
                self.mark_selected_seen();
            }
            NormalAction::HalfPageUp => {
                let before = self.list_state.selected();
                self.select_half_page_up();
                if self.visual_select_active {
                    self.extend_selection_range(before, self.list_state.selected());
                }
                self.mark_selected_seen();
            }
            NormalAction::GoToTop => {
                let before = self.list_state.selected();
                self.select_first();
                if self.visual_select_active {
                    self.extend_selection_range(before, self.list_state.selected());
                }
                self.mark_selected_seen();
            }
            NormalAction::GoToBottom => {
                let before = self.list_state.selected();
                self.select_last();
                if self.visual_select_active {
                    self.extend_selection_range(before, self.list_state.selected());
                }
                self.mark_selected_seen();
            }
            NormalAction::ShrinkList => {
                self.list_ratio = (self.list_ratio.saturating_sub(5)).max(10);
            }
            NormalAction::GrowList => {
                self.list_ratio = (self.list_ratio + 5).min(90);
            }
            NormalAction::ShowHelp => {
                self.show_help_overlay = true;
                self.help_scroll = 0;
            }
            NormalAction::ToggleSelect => {
                self.visual_select_active = false;
                if let Some(idx) = self.list_state.selected() {
                    if let Some(prompt) = self.orch.prompts.get(idx) {
                        let id = prompt.id;
                        if !self.selected_ids.remove(&id) {
                            self.selected_ids.insert(id);
                        }
                    }
                }
            }
            NormalAction::SelectAllVisible => {
                let visible_ids: Vec<usize> = self
                    .visible_prompt_indices()
                    .iter()
                    .filter_map(|&idx| self.orch.prompts.get(idx).map(|p| p.id))
                    .collect();
                let all_selected = visible_ids.iter().all(|id| self.selected_ids.contains(id));
                if all_selected {
                    for id in &visible_ids {
                        self.selected_ids.remove(id);
                    }
                } else {
                    for id in visible_ids {
                        self.selected_ids.insert(id);
                    }
                }
            }
            NormalAction::VisualSelect => {
                self.visual_select_active = !self.visual_select_active;
                if self.visual_select_active {
                    if let Some(idx) = self.list_state.selected() {
                        if let Some(prompt) = self.orch.prompts.get(idx) {
                            self.selected_ids.insert(prompt.id);
                        }
                    }
                }
            }
            NormalAction::DeleteSelected => {
                if self.selected_ids.is_empty() {
                    if let Some(idx) = self.list_state.selected() {
                        if let Some(prompt) = self.orch.prompts.get(idx) {
                            self.selected_ids.insert(prompt.id);
                        }
                    }
                }
                if !self.selected_ids.is_empty() {
                    self.confirm_batch_delete = true;
                }
            }
            NormalAction::KillSelected => {
                if !self.selected_ids.is_empty() {
                    self.batch_kill();
                } else {
                    let kill_id = self.selected_prompt().and_then(|p| {
                        if p.status == PromptStatus::Running || p.status == PromptStatus::Idle {
                            Some(p.id)
                        } else {
                            None
                        }
                    });
                    if let Some(id) = kill_id {
                        self.orch.kill_worker(id);
                    }
                }
            }
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
        // Ctrl+W toggles worktree mode for the current prompt
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.worktree_pending = !self.worktree_pending;
            return;
        }

        // Shift+Enter or Alt+Enter → insert newline
        if key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::ALT))
        {
            self.input.insert_newline();
            self.history_index = None;
            self.suggestions.clear();
            self.template_suggestions.clear();
            return;
        }

        // Ctrl+E → open external editor
        if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_external_editor = true;
            return;
        }

        // Up/Down: navigate within multi-line buffer before falling through to history
        if key.code == KeyCode::Up
            && self.suggestions.is_empty()
            && self.template_suggestions.is_empty()
            && self.input.is_multiline()
        {
            self.input.move_up();
            return;
        }
        if key.code == KeyCode::Down
            && self.suggestions.is_empty()
            && self.template_suggestions.is_empty()
            && self.input.is_multiline()
        {
            self.input.move_down();
            return;
        }

        if let Some(action) = self.keymap.insert.get(&key.code) {
            match action {
                InsertAction::Cancel => {
                    self.mode = AppMode::Normal;
                    self.input.clear();
                    self.suggestions.clear();
                    self.suggestion_index = 0;
                    self.history_index = None;
                    self.history_stash.clear();
                    self.template_suggestions.clear();
                    self.template_suggestion_index = 0;
                    self.worktree_pending = false;
                }
                InsertAction::Submit => {
                    let text = self.input.trimmed();
                    if !text.is_empty() {
                        let (cwd, prompt_text) = Self::parse_cwd_prefix(&text);
                        if !prompt_text.is_empty() {
                            let (tags, clean_text) = clhorde_core::prompt::parse_tags(&prompt_text);
                            if !clean_text.is_empty() {
                                self.add_prompt(clean_text, cwd, self.worktree_pending, tags);
                            }
                            self.append_history(&text);
                        }
                    }
                    self.input.clear();
                    self.suggestions.clear();
                    self.suggestion_index = 0;
                    self.history_index = None;
                    self.history_stash.clear();
                    self.template_suggestions.clear();
                    self.template_suggestion_index = 0;
                    self.worktree_pending = false;
                    self.mode = AppMode::Normal;
                }
                InsertAction::AcceptSuggestion => {
                    if !self.suggestions.is_empty() {
                        self.accept_suggestion();
                    } else if !self.template_suggestions.is_empty() {
                        self.accept_template_suggestion();
                    }
                }
                InsertAction::NextSuggestion => {
                    if !self.suggestions.is_empty() {
                        self.suggestion_index =
                            (self.suggestion_index + 1) % self.suggestions.len();
                    } else if !self.template_suggestions.is_empty() {
                        self.template_suggestion_index =
                            (self.template_suggestion_index + 1) % self.template_suggestions.len();
                    } else {
                        self.history_prev();
                    }
                }
                InsertAction::PrevSuggestion => {
                    if !self.suggestions.is_empty() {
                        self.suggestion_index = if self.suggestion_index == 0 {
                            self.suggestions.len() - 1
                        } else {
                            self.suggestion_index - 1
                        };
                    } else if !self.template_suggestions.is_empty() {
                        self.template_suggestion_index = if self.template_suggestion_index == 0 {
                            self.template_suggestions.len() - 1
                        } else {
                            self.template_suggestion_index - 1
                        };
                    } else {
                        self.history_next();
                    }
                }
            }
            return;
        }
        // Text input fallthrough
        match key.code {
            KeyCode::Backspace => {
                self.input.backspace();
                self.history_index = None;
                self.update_suggestions();
                self.update_template_suggestions();
            }
            KeyCode::Delete => {
                self.input.delete();
            }
            KeyCode::Left => {
                self.input.move_left();
            }
            KeyCode::Right => {
                self.input.move_right();
            }
            KeyCode::Home => {
                self.input.move_home();
            }
            KeyCode::End => {
                self.input.move_end();
            }
            KeyCode::Char(c) => {
                self.input.insert_char(c);
                self.history_index = None;
                self.update_suggestions();
                self.update_template_suggestions();
            }
            _ => {}
        }
    }

    fn handle_view_key(&mut self, key: KeyEvent) {
        // Ctrl+P toggles quick prompts popup
        if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_quick_prompts_popup = !self.show_quick_prompts_popup;
            return;
        }

        // When popup is visible, any other key closes it
        if self.show_quick_prompts_popup {
            self.show_quick_prompts_popup = false;
            if key.code == KeyCode::Esc {
                return;
            }
        }

        // View actions take priority
        let Some(action) = self.keymap.view.get(&key.code) else {
            self.try_quick_prompt(&key);
            return;
        };
        match action {
            ViewAction::Back => {
                self.show_quick_prompts_popup = false;
                self.mode = AppMode::Normal;
                self.scroll_offset = 0;
                self.list_collapsed = false;
            }
            ViewAction::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            ViewAction::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            ViewAction::Interact => {
                let target_mode = {
                    let prompt_info = self.selected_prompt().map(|p| (p.id, p.status.clone()));
                    prompt_info.and_then(|(id, status)| {
                        if status == PromptStatus::Running || status == PromptStatus::Idle {
                            if self.orch.pty_handles.contains_key(&id) {
                                Some(AppMode::PtyInteract)
                            } else {
                                Some(AppMode::Interact)
                            }
                        } else {
                            None
                        }
                    })
                };
                if let Some(mode) = target_mode {
                    self.show_quick_prompts_popup = false;
                    if mode == AppMode::Interact {
                        self.interact_input.clear();
                    }
                    self.mode = mode;
                    self.list_collapsed = true;
                }
            }
            ViewAction::ToggleAutoscroll => {
                self.auto_scroll = !self.auto_scroll;
            }
            ViewAction::KillWorker => {
                let kill_id = self.selected_prompt().and_then(|p| {
                    if p.status == PromptStatus::Running || p.status == PromptStatus::Idle {
                        Some(p.id)
                    } else {
                        None
                    }
                });
                if let Some(id) = kill_id {
                    self.orch.kill_worker(id);
                }
            }
            ViewAction::Export => {
                self.export_selected_output();
            }
            ViewAction::ToggleSplit => {
                self.list_collapsed = !self.list_collapsed;
            }
        }
    }

    fn handle_interact_key(&mut self, key: KeyEvent) {
        if let Some(action) = self.keymap.interact.get(&key.code) {
            match action {
                InteractAction::Back => {
                    self.mode = AppMode::Normal;
                    self.interact_input.clear();
                    self.list_collapsed = false;
                }
                InteractAction::Send => {
                    if let Some(idx) = self.list_state.selected() {
                        let text = self.interact_input.clone();
                        self.orch.send_interact_input(idx, &text);
                    }
                    self.interact_input.clear();
                }
            }
            return;
        }
        // Text input fallthrough
        match key.code {
            KeyCode::Backspace => {
                self.interact_input.pop();
            }
            KeyCode::Char(c) => {
                self.interact_input.push(c);
            }
            _ => {}
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        if let Some(action) = self.keymap.filter.get(&key.code) {
            match action {
                FilterAction::Confirm => {
                    let text = self.filter_input.trim().to_string();
                    if text.is_empty() {
                        self.filter_text = None;
                    } else {
                        self.filter_text = Some(text);
                    }
                    self.rebuild_filter();
                    self.mode = AppMode::Normal;
                    self.clamp_selection_to_filter();
                }
                FilterAction::Cancel => {
                    self.filter_text = None;
                    self.filter_input.clear();
                    self.rebuild_filter();
                    self.mode = AppMode::Normal;
                }
            }
            return;
        }
        match key.code {
            KeyCode::Backspace => {
                self.filter_input.pop();
                let text = self.filter_input.trim().to_string();
                self.filter_text = if text.is_empty() { None } else { Some(text) };
                self.rebuild_filter();
                self.clamp_selection_to_filter();
            }
            KeyCode::Char(c) => {
                self.filter_input.push(c);
                let text = self.filter_input.trim().to_string();
                self.filter_text = if text.is_empty() { None } else { Some(text) };
                self.rebuild_filter();
                self.clamp_selection_to_filter();
            }
            _ => {}
        }
    }

    fn handle_pty_interact_key(&mut self, key: KeyEvent) {
        // Esc exits PTY interact mode back to view
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            self.mode = AppMode::ViewOutput;
            return;
        }

        // If prompt is no longer running, exit back to view
        if let Some(prompt) = self.selected_prompt() {
            if prompt.status != PromptStatus::Running && prompt.status != PromptStatus::Idle {
                self.mode = AppMode::ViewOutput;
                return;
            }
        }

        // Forward all other keys to PTY as raw bytes
        let bytes = pty_worker::key_event_to_bytes(key);
        if bytes.is_empty() {
            return;
        }
        if let Some(prompt) = self.selected_prompt() {
            let id = prompt.id;
            self.orch.send_pty_bytes(id, bytes);
        }
    }

    fn try_quick_prompt(&mut self, key: &KeyEvent) {
        let Some(message) = self.keymap.quick_prompts.get(&key.code) else {
            return;
        };
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let Some(prompt) = self.orch.prompts.get(idx) else {
            return;
        };
        if prompt.status != PromptStatus::Running && prompt.status != PromptStatus::Idle {
            return;
        }
        let id = prompt.id;
        let is_pty = self.orch.pty_handles.contains_key(&id);
        self.orch.try_quick_prompt_send(id, message, is_pty);
    }

    fn select_next(&mut self) {
        if self.orch.prompts.is_empty() {
            return;
        }
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            let current = self.list_state.selected().unwrap_or(0);
            let current_filter_pos = self
                .filtered_indices
                .iter()
                .position(|&i| i == current)
                .unwrap_or(0);
            let next_pos = (current_filter_pos + 1).min(self.filtered_indices.len() - 1);
            self.list_state
                .select(Some(self.filtered_indices[next_pos]));
        } else {
            let i = match self.list_state.selected() {
                Some(i) => (i + 1).min(self.orch.prompts.len() - 1),
                None => 0,
            };
            self.list_state.select(Some(i));
        }
    }

    fn select_prev(&mut self) {
        if self.orch.prompts.is_empty() {
            return;
        }
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            let current = self.list_state.selected().unwrap_or(0);
            let current_filter_pos = self
                .filtered_indices
                .iter()
                .position(|&i| i == current)
                .unwrap_or(0);
            let prev_pos = current_filter_pos.saturating_sub(1);
            self.list_state
                .select(Some(self.filtered_indices[prev_pos]));
        } else {
            let i = match self.list_state.selected() {
                Some(i) => i.saturating_sub(1),
                None => 0,
            };
            self.list_state.select(Some(i));
        }
    }

    fn half_page_size(&self) -> usize {
        let h = if self.list_height > 2 { self.list_height - 2 } else { 10 };
        (h as usize / 2).max(1)
    }

    fn select_half_page_down(&mut self) {
        if self.orch.prompts.is_empty() {
            return;
        }
        let step = self.half_page_size();
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            let current = self.list_state.selected().unwrap_or(0);
            let current_filter_pos = self
                .filtered_indices
                .iter()
                .position(|&i| i == current)
                .unwrap_or(0);
            let next_pos = (current_filter_pos + step).min(self.filtered_indices.len() - 1);
            self.list_state
                .select(Some(self.filtered_indices[next_pos]));
        } else {
            let i = match self.list_state.selected() {
                Some(i) => (i + step).min(self.orch.prompts.len() - 1),
                None => 0,
            };
            self.list_state.select(Some(i));
        }
    }

    fn select_half_page_up(&mut self) {
        if self.orch.prompts.is_empty() {
            return;
        }
        let step = self.half_page_size();
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            let current = self.list_state.selected().unwrap_or(0);
            let current_filter_pos = self
                .filtered_indices
                .iter()
                .position(|&i| i == current)
                .unwrap_or(0);
            let prev_pos = current_filter_pos.saturating_sub(step);
            self.list_state
                .select(Some(self.filtered_indices[prev_pos]));
        } else {
            let i = match self.list_state.selected() {
                Some(i) => i.saturating_sub(step),
                None => 0,
            };
            self.list_state.select(Some(i));
        }
    }

    fn select_first(&mut self) {
        if self.orch.prompts.is_empty() {
            return;
        }
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            self.list_state
                .select(Some(self.filtered_indices[0]));
        } else {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if self.orch.prompts.is_empty() {
            return;
        }
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            self.list_state
                .select(Some(*self.filtered_indices.last().unwrap()));
        } else {
            self.list_state.select(Some(self.orch.prompts.len() - 1));
        }
    }

    // ── Worktree cleanup ── (handled by Orchestrator now)

    // ── Feature 1: Export ──

    fn export_selected_output(&mut self) {
        let Some(prompt) = self.selected_prompt() else {
            self.status_message = Some(("No prompt selected".to_string(), Instant::now()));
            return;
        };
        let output = prompt.output.clone().unwrap_or_default();
        if output.is_empty() {
            self.status_message = Some(("No output to export".to_string(), Instant::now()));
            return;
        }

        let id = prompt.id;
        let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let filename = home.join(format!("clhorde-output-{id}-{timestamp}.md"));

        let header = format!("# clhorde output #{id}\n\nPrompt: {}\n\n---\n\n", prompt.text);
        let content = format!("{header}{output}");

        match fs::write(&filename, &content) {
            Ok(_) => {
                self.status_message = Some((
                    format!("Saved to {}", filename.display()),
                    Instant::now(),
                ));
            }
            Err(e) => {
                self.status_message =
                    Some((format!("Export failed: {e}"), Instant::now()));
            }
        }
    }

    // ── Feature 2: Retry ──

    fn retry_selected(&mut self) {
        let Some(prompt) = self.selected_prompt() else {
            return;
        };
        if prompt.status != PromptStatus::Completed && prompt.status != PromptStatus::Failed {
            return;
        }
        let text = prompt.text.clone();
        let cwd = prompt.cwd.clone();
        let mode = prompt.mode;
        let wt = prompt.worktree;
        let tags = prompt.tags.clone();
        self.orch.retry_prompt(text, cwd, mode, wt, tags);
        self.rebuild_filter();
    }

    fn resume_selected(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        self.orch.resume_prompt(idx);
    }

    // ── Feature 4: Reorder ──

    fn move_selected_up(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        match self.orch.move_prompt_up(idx) {
            MoveResult::Moved { prompt_id } => {
                self.list_state.select(Some(idx - 1));
                self.recently_moved = Some((prompt_id, Instant::now()));
                self.status_message = Some((format!("Moved #{prompt_id} up"), Instant::now()));
                self.rebuild_filter();
            }
            MoveResult::NotMoved => {}
        }
    }

    fn move_selected_down(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        match self.orch.move_prompt_down(idx) {
            MoveResult::Moved { prompt_id } => {
                self.list_state.select(Some(idx + 1));
                self.recently_moved = Some((prompt_id, Instant::now()));
                self.status_message = Some((format!("Moved #{prompt_id} down"), Instant::now()));
                self.rebuild_filter();
            }
            MoveResult::NotMoved => {}
        }
    }

    // ── Feature 5: Filter ──

    fn rebuild_filter(&mut self) {
        self.filtered_indices = match &self.filter_text {
            Some(filter) => {
                let mut tag_filters = Vec::new();
                let mut text_parts = Vec::new();
                for word in filter.split_whitespace() {
                    if let Some(tag) = word.strip_prefix('@') {
                        if !tag.is_empty() {
                            tag_filters.push(tag.to_lowercase());
                        }
                    } else {
                        text_parts.push(word.to_lowercase());
                    }
                }
                let text_filter = text_parts.join(" ");

                self.orch
                    .prompts
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        let tags_match = tag_filters.iter().all(|tf| {
                            p.tags.iter().any(|t| t.to_lowercase() == *tf)
                        });
                        let text_match = text_filter.is_empty()
                            || p.text.to_lowercase().contains(&text_filter);
                        tags_match && text_match
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
            None => (0..self.orch.prompts.len()).collect(),
        };
    }

    fn clamp_selection_to_filter(&mut self) {
        if self.filtered_indices.is_empty() {
            self.list_state.select(None);
        } else if let Some(current) = self.list_state.selected() {
            if !self.filtered_indices.contains(&current) {
                self.list_state.select(Some(self.filtered_indices[0]));
            }
        } else {
            self.list_state.select(Some(self.filtered_indices[0]));
        }
    }

    /// Get the indices of prompts to display (respects filter).
    pub fn visible_prompt_indices(&self) -> &[usize] {
        &self.filtered_indices
    }

    // ── Selection utilities ──

    pub fn clear_selection(&mut self) {
        self.selected_ids.clear();
        self.visual_select_active = false;
    }

    pub fn selection_count(&self) -> usize {
        self.selected_ids.len()
    }

    pub fn is_selected(&self, prompt_id: usize) -> bool {
        self.selected_ids.contains(&prompt_id)
    }

    /// Add all prompts between `from` and `to` indices (inclusive) to selection.
    fn extend_selection_range(&mut self, from: Option<usize>, to: Option<usize>) {
        let (Some(a), Some(b)) = (from, to) else {
            return;
        };
        let lo = a.min(b);
        let hi = a.max(b);
        for idx in lo..=hi {
            if let Some(prompt) = self.orch.prompts.get(idx) {
                self.selected_ids.insert(prompt.id);
            }
        }
    }

    // ── Batch operations ──

    fn batch_retry(&mut self) {
        let count = self.orch.batch_retry_prompts(&self.selected_ids);
        self.clear_selection();
        self.rebuild_filter();
        if count > 0 {
            self.status_message = Some((format!("Retried {count} prompts"), Instant::now()));
        }
    }

    fn batch_kill(&mut self) {
        let count = self.orch.batch_kill_prompts(&self.selected_ids);
        self.clear_selection();
        if count > 0 {
            self.status_message = Some((format!("Killed {count} workers"), Instant::now()));
        }
    }

    fn execute_batch_delete(&mut self) {
        let count = self.orch.batch_delete_prompts(&self.selected_ids);
        self.clear_selection();
        self.rebuild_filter();
        self.clamp_selection_to_filter();
        if count > 0 {
            self.status_message = Some((format!("Deleted {count} prompts"), Instant::now()));
        }
    }

    fn batch_toggle_mode(&mut self) {
        let count = self.orch.batch_toggle_mode_prompts(&self.selected_ids);
        self.clear_selection();
        if count > 0 {
            self.status_message =
                Some((format!("Toggled mode on {count} prompts"), Instant::now()));
        }
    }

    // ── Feature 6: History ──

    fn append_history(&mut self, text: &str) {
        if self.history.last().map(|s| s.as_str()) == Some(text) {
            return;
        }
        self.history.push(text.to_string());
        clhorde_core::config::append_history(text);
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_stash = self.input.to_string();
                let idx = self.history.len() - 1;
                self.history_index = Some(idx);
                self.input.set(&self.history[idx].clone());
            }
            Some(idx) => {
                if idx > 0 {
                    let new_idx = idx - 1;
                    self.history_index = Some(new_idx);
                    self.input.set(&self.history[new_idx].clone());
                }
            }
        }
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        if idx + 1 < self.history.len() {
            let new_idx = idx + 1;
            self.history_index = Some(new_idx);
            self.input.set(&self.history[new_idx].clone());
        } else {
            self.history_index = None;
            let stash = self.history_stash.clone();
            self.input.set(&stash);
            self.history_stash.clear();
        }
    }

    // ── Feature 8: Templates ──

    fn update_template_suggestions(&mut self) {
        self.template_suggestions.clear();
        self.template_suggestion_index = 0;

        if self.templates.is_empty() {
            return;
        }

        let input_str = self.input.first_line().to_string();
        let input = &input_str;
        if !input.starts_with(':') {
            return;
        }

        let prefix = &input[1..];
        if prefix.contains(' ') {
            return;
        }

        let mut matches: Vec<String> = self
            .templates
            .keys()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect();
        matches.sort();
        if matches.len() > 10 {
            matches.truncate(10);
        }
        self.template_suggestions = matches;
    }

    fn accept_template_suggestion(&mut self) {
        if let Some(name) = self.template_suggestions.get(self.template_suggestion_index).cloned() {
            if let Some(template_text) = self.templates.get(&name).cloned() {
                self.input.set(&format!("{template_text} "));
                self.template_suggestions.clear();
                self.template_suggestion_index = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::orchestrator::Orchestrator;
    use crate::worker::WorkerMessage;
    use clhorde_core::prompt::PromptMode;

    /// Test-only constructor that skips persistence loading.
    fn new_test_app() -> App {
        let orch = Orchestrator::new_test();
        let mut list_state = ListState::default();
        list_state.select(None);
        App {
            orch,
            mode: AppMode::Normal,
            list_state,
            input: TextBuffer::new(),
            open_external_editor: false,
            scroll_offset: 0,
            should_quit: false,
            interact_input: String::new(),
            auto_scroll: true,
            suggestions: Vec::new(),
            suggestion_index: 0,
            tick: 0,
            keymap: Keymap::default(),
            status_message: None,
            confirm_quit: false,
            filter_text: None,
            filter_input: String::new(),
            filtered_indices: Vec::new(),
            history: Vec::new(),
            history_index: None,
            history_stash: String::new(),
            templates: std::collections::HashMap::new(),
            template_suggestions: Vec::new(),
            template_suggestion_index: 0,
            show_quick_prompts_popup: false,
            output_panel_size: None,
            last_pty_size: None,
            worktree_pending: false,
            list_height: 0,
            pending_g: false,
            list_ratio: 40,
            list_collapsed: false,
            session_start: Instant::now(),
            show_help_overlay: false,
            help_scroll: 0,
            recently_moved: None,
            selected_ids: HashSet::new(),
            visual_select_active: false,
            confirm_batch_delete: false,
        }
    }

    // ── App::new defaults ──

    #[test]
    fn app_new_defaults() {
        let app = new_test_app();
        assert_eq!(app.orch.max_workers, 3);
        assert_eq!(app.orch.active_workers, 0);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.auto_scroll);
        assert!(app.orch.prompts.is_empty());
        assert_eq!(app.orch.next_id, 1);
        assert!(!app.should_quit);
        assert!(!app.confirm_quit);
        assert_eq!(app.orch.default_mode, PromptMode::Interactive);
        assert!(app.filter_text.is_none());
        assert!(app.history_index.is_none());
    }

    // ── add_prompt / pending_count / completed_count ──

    #[test]
    fn add_prompt_increments_id() {
        let mut app = new_test_app();
        app.add_prompt("first".to_string(), None, false, Vec::new());
        app.add_prompt("second".to_string(), None, false, Vec::new());
        app.add_prompt("third".to_string(), None, false, Vec::new());

        assert_eq!(app.orch.prompts.len(), 3);
        assert_eq!(app.orch.prompts[0].id, 1);
        assert_eq!(app.orch.prompts[1].id, 2);
        assert_eq!(app.orch.prompts[2].id, 3);
        assert_eq!(app.orch.next_id, 4);
    }

    #[test]
    fn add_prompt_selects_first() {
        let mut app = new_test_app();
        assert!(app.list_state.selected().is_none());
        app.add_prompt("test".to_string(), None, false, Vec::new());
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn pending_and_completed_counts() {
        let mut app = new_test_app();
        app.add_prompt("a".to_string(), None, false, Vec::new());
        app.add_prompt("b".to_string(), None, false, Vec::new());
        app.add_prompt("c".to_string(), None, false, Vec::new());

        assert_eq!(app.pending_count(), 3);
        assert_eq!(app.completed_count(), 0);

        app.orch.prompts[0].status = PromptStatus::Completed;
        app.orch.prompts[1].status = PromptStatus::Failed;

        assert_eq!(app.pending_count(), 1);
        assert_eq!(app.completed_count(), 2);
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_empty_list() {
        let mut app = new_test_app();
        app.select_next(); // should not panic
        assert!(app.list_state.selected().is_none());
    }

    #[test]
    fn select_prev_empty_list() {
        let mut app = new_test_app();
        app.select_prev(); // should not panic
        assert!(app.list_state.selected().is_none());
    }

    #[test]
    fn select_next_clamps_to_end() {
        let mut app = new_test_app();
        app.add_prompt("a".to_string(), None, false, Vec::new());
        app.add_prompt("b".to_string(), None, false, Vec::new());
        app.list_state.select(Some(1));

        app.select_next();
        assert_eq!(app.list_state.selected(), Some(1)); // stays at end
    }

    #[test]
    fn select_prev_clamps_to_start() {
        let mut app = new_test_app();
        app.add_prompt("a".to_string(), None, false, Vec::new());
        app.add_prompt("b".to_string(), None, false, Vec::new());
        app.list_state.select(Some(0));

        app.select_prev();
        assert_eq!(app.list_state.selected(), Some(0)); // stays at start
    }

    #[test]
    fn select_next_advances() {
        let mut app = new_test_app();
        app.add_prompt("a".to_string(), None, false, Vec::new());
        app.add_prompt("b".to_string(), None, false, Vec::new());
        app.add_prompt("c".to_string(), None, false, Vec::new());
        app.list_state.select(Some(0));

        app.select_next();
        assert_eq!(app.list_state.selected(), Some(1));
        app.select_next();
        assert_eq!(app.list_state.selected(), Some(2));
    }

    #[test]
    fn select_prev_goes_back() {
        let mut app = new_test_app();
        app.add_prompt("a".to_string(), None, false, Vec::new());
        app.add_prompt("b".to_string(), None, false, Vec::new());
        app.list_state.select(Some(1));

        app.select_prev();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    // ── move_selected_up / move_selected_down ──

    fn app_with_prompts(texts: &[&str]) -> App {
        let mut app = new_test_app();
        for t in texts {
            app.add_prompt(t.to_string(), None, false, Vec::new());
        }
        app
    }

    #[test]
    fn move_down_swaps_pending() {
        let mut app = app_with_prompts(&["a", "b", "c"]);
        app.list_state.select(Some(0));

        app.move_selected_down();
        assert_eq!(app.orch.prompts[0].text, "b");
        assert_eq!(app.orch.prompts[1].text, "a");
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn move_up_swaps_pending() {
        let mut app = app_with_prompts(&["a", "b", "c"]);
        app.list_state.select(Some(2));

        app.move_selected_up();
        assert_eq!(app.orch.prompts[1].text, "c");
        assert_eq!(app.orch.prompts[2].text, "b");
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn move_down_at_end_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.list_state.select(Some(1));

        app.move_selected_down();
        assert_eq!(app.orch.prompts[0].text, "a");
        assert_eq!(app.orch.prompts[1].text, "b");
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn move_up_at_start_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.list_state.select(Some(0));

        app.move_selected_up();
        assert_eq!(app.orch.prompts[0].text, "a");
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn move_non_pending_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.orch.prompts[0].status = PromptStatus::Running;
        app.list_state.select(Some(0));

        app.move_selected_down();
        assert_eq!(app.orch.prompts[0].text, "a");
        assert_eq!(app.orch.prompts[1].text, "b");
    }

    #[test]
    fn move_no_selection_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.list_state.select(None);
        app.move_selected_up(); // should not panic
        app.move_selected_down(); // should not panic
    }

    // ── retry_selected ──

    #[test]
    fn retry_completed_creates_new_prompt() {
        let mut app = app_with_prompts(&["hello world"]);
        app.orch.prompts[0].status = PromptStatus::Completed;
        app.orch.prompts[0].cwd = Some("/tmp".to_string());
        app.list_state.select(Some(0));

        app.retry_selected();

        assert_eq!(app.orch.prompts.len(), 2);
        let retried = &app.orch.prompts[1];
        assert_eq!(retried.text, "hello world");
        assert_eq!(retried.cwd, Some("/tmp".to_string()));
        assert_eq!(retried.status, PromptStatus::Pending);
        assert!(retried.id > app.orch.prompts[0].id);
    }

    #[test]
    fn retry_failed_creates_new_prompt() {
        let mut app = app_with_prompts(&["fail"]);
        app.orch.prompts[0].status = PromptStatus::Failed;
        app.list_state.select(Some(0));

        app.retry_selected();
        assert_eq!(app.orch.prompts.len(), 2);
        assert_eq!(app.orch.prompts[1].status, PromptStatus::Pending);
    }

    #[test]
    fn retry_running_is_noop() {
        let mut app = app_with_prompts(&["running"]);
        app.orch.prompts[0].status = PromptStatus::Running;
        app.list_state.select(Some(0));

        app.retry_selected();
        assert_eq!(app.orch.prompts.len(), 1);
    }

    #[test]
    fn retry_pending_is_noop() {
        let mut app = app_with_prompts(&["pending"]);
        app.list_state.select(Some(0));

        app.retry_selected();
        assert_eq!(app.orch.prompts.len(), 1);
    }

    #[test]
    fn retry_no_selection_is_noop() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.prompts[0].status = PromptStatus::Completed;
        app.list_state.select(None);

        app.retry_selected();
        assert_eq!(app.orch.prompts.len(), 1);
    }

    // ── rebuild_filter ──

    #[test]
    fn filter_no_text_includes_all() {
        let mut app = app_with_prompts(&["foo", "bar", "baz"]);
        app.filter_text = None;
        app.rebuild_filter();
        assert_eq!(app.filtered_indices, vec![0, 1, 2]);
    }

    #[test]
    fn filter_matches_case_insensitive() {
        let mut app = app_with_prompts(&["Hello World", "goodbye", "HELLO again"]);
        app.filter_text = Some("hello".to_string());
        app.rebuild_filter();
        assert_eq!(app.filtered_indices, vec![0, 2]);
    }

    #[test]
    fn filter_no_matches() {
        let mut app = app_with_prompts(&["foo", "bar"]);
        app.filter_text = Some("xyz".to_string());
        app.rebuild_filter();
        assert!(app.filtered_indices.is_empty());
    }

    #[test]
    fn filter_partial_match() {
        let mut app = app_with_prompts(&["refactor auth", "fix auth bug", "add tests"]);
        app.filter_text = Some("auth".to_string());
        app.rebuild_filter();
        assert_eq!(app.filtered_indices, vec![0, 1]);
    }

    #[test]
    fn clamp_selection_when_filtered_out() {
        let mut app = app_with_prompts(&["foo", "bar", "baz"]);
        app.list_state.select(Some(1)); // "bar" selected
        app.filter_text = Some("foo".to_string());
        app.rebuild_filter();
        app.clamp_selection_to_filter();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn clamp_selection_empty_filter_result() {
        let mut app = app_with_prompts(&["foo"]);
        app.list_state.select(Some(0));
        app.filter_text = Some("xyz".to_string());
        app.rebuild_filter();
        app.clamp_selection_to_filter();
        assert!(app.list_state.selected().is_none());
    }

    // ── tag filtering ──

    #[test]
    fn filter_by_tag() {
        let mut app = new_test_app();
        app.add_prompt("Fix navbar".to_string(), None, false, vec!["frontend".to_string()]);
        app.add_prompt("Fix API".to_string(), None, false, vec!["backend".to_string()]);
        app.add_prompt("Fix styles".to_string(), None, false, vec!["frontend".to_string()]);
        app.filter_text = Some("@frontend".to_string());
        app.rebuild_filter();
        assert_eq!(app.filtered_indices, vec![0, 2]);
    }

    #[test]
    fn filter_by_tag_and_text() {
        let mut app = new_test_app();
        app.add_prompt("Fix navbar".to_string(), None, false, vec!["frontend".to_string()]);
        app.add_prompt("Fix styles".to_string(), None, false, vec!["frontend".to_string()]);
        app.add_prompt("Fix API".to_string(), None, false, vec!["backend".to_string()]);
        app.filter_text = Some("@frontend navbar".to_string());
        app.rebuild_filter();
        assert_eq!(app.filtered_indices, vec![0]);
    }

    #[test]
    fn filter_by_multiple_tags() {
        let mut app = new_test_app();
        app.add_prompt("Fix".to_string(), None, false, vec!["frontend".to_string(), "urgent".to_string()]);
        app.add_prompt("Fix2".to_string(), None, false, vec!["frontend".to_string()]);
        app.add_prompt("Fix3".to_string(), None, false, vec!["backend".to_string()]);
        app.filter_text = Some("@frontend @urgent".to_string());
        app.rebuild_filter();
        assert_eq!(app.filtered_indices, vec![0]);
    }

    // ── history_prev / history_next ──

    #[test]
    fn history_empty_is_noop() {
        let mut app = new_test_app();
        app.history.clear();
        app.input.set("current");
        app.history_prev();
        assert_eq!(app.input, "current");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn history_prev_stashes_and_navigates() {
        let mut app = new_test_app();
        app.history = vec!["first".to_string(), "second".to_string()];
        app.input.set("typing");

        app.history_prev();
        assert_eq!(app.input, "second");
        assert_eq!(app.history_index, Some(1));
        assert_eq!(app.history_stash, "typing");

        app.history_prev();
        assert_eq!(app.input, "first");
        assert_eq!(app.history_index, Some(0));
    }

    #[test]
    fn history_prev_stops_at_beginning() {
        let mut app = new_test_app();
        app.history = vec!["only".to_string()];
        app.input.set("typing");

        app.history_prev();
        assert_eq!(app.input, "only");
        assert_eq!(app.history_index, Some(0));

        app.history_prev(); // already at start
        assert_eq!(app.input, "only");
        assert_eq!(app.history_index, Some(0));
    }

    #[test]
    fn history_next_restores_stash() {
        let mut app = new_test_app();
        app.history = vec!["first".to_string(), "second".to_string()];
        app.input.set("typing");

        app.history_prev(); // "second"
        app.history_next(); // past end -> restore stash
        assert_eq!(app.input, "typing");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn history_next_without_navigating_is_noop() {
        let mut app = new_test_app();
        app.history = vec!["first".to_string()];
        app.input.set("current");
        app.history_next();
        assert_eq!(app.input, "current");
    }

    #[test]
    fn history_prev_next_roundtrip() {
        let mut app = new_test_app();
        app.history = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        app.input.set("now");

        app.history_prev(); // "c"
        app.history_prev(); // "b"
        assert_eq!(app.input, "b");

        app.history_next(); // "c"
        assert_eq!(app.input, "c");

        app.history_next(); // restore "now"
        assert_eq!(app.input, "now");
        assert!(app.history_index.is_none());
    }

    // ── parse_cwd_prefix ──

    #[test]
    fn parse_cwd_with_valid_dir() {
        let (cwd, text) = App::parse_cwd_prefix("/tmp: do something");
        assert_eq!(cwd, Some("/tmp".to_string()));
        assert_eq!(text, "do something");
    }

    #[test]
    fn parse_cwd_with_nonexistent_dir() {
        let (cwd, text) = App::parse_cwd_prefix("/nonexistent_xyz_123: do something");
        assert!(cwd.is_none());
        assert_eq!(text, "/nonexistent_xyz_123: do something");
    }

    #[test]
    fn parse_cwd_no_colon() {
        let (cwd, text) = App::parse_cwd_prefix("plain text here");
        assert!(cwd.is_none());
        assert_eq!(text, "plain text here");
    }

    #[test]
    fn parse_cwd_empty_prefix() {
        let (cwd, text) = App::parse_cwd_prefix(": after colon");
        assert!(cwd.is_none());
        assert_eq!(text, ": after colon");
    }

    // ── apply_message ──

    #[test]
    fn apply_output_chunk() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.prompts[0].status = PromptStatus::Running;

        app.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "hello ".to_string(),
        });
        app.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "world".to_string(),
        });

        assert_eq!(app.orch.prompts[0].output, Some("hello world".to_string()));
    }

    #[test]
    fn apply_turn_complete_transitions_to_idle() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.prompts[0].status = PromptStatus::Running;
        app.orch.prompts[0].output = Some("output".to_string());

        app.apply_message(WorkerMessage::TurnComplete { prompt_id: 1 });

        assert_eq!(app.orch.prompts[0].status, PromptStatus::Idle);
        assert_eq!(app.orch.prompts[0].output, Some("output\n".to_string()));
    }

    #[test]
    fn apply_finished_success() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.prompts[0].status = PromptStatus::Running;
        app.orch.active_workers = 1;

        app.apply_message(WorkerMessage::Finished {
            prompt_id: 1,
            exit_code: Some(0),
        });

        assert_eq!(app.orch.prompts[0].status, PromptStatus::Completed);
        assert!(app.orch.prompts[0].finished_at_ms.is_some());
        assert_eq!(app.orch.active_workers, 0);
    }

    #[test]
    fn apply_finished_failure() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.prompts[0].status = PromptStatus::Running;
        app.orch.active_workers = 1;

        app.apply_message(WorkerMessage::Finished {
            prompt_id: 1,
            exit_code: Some(1),
        });

        assert_eq!(app.orch.prompts[0].status, PromptStatus::Failed);
        assert!(app.orch.prompts[0].error.is_some());
    }

    #[test]
    fn apply_spawn_error() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.active_workers = 1;

        app.apply_message(WorkerMessage::SpawnError {
            prompt_id: 1,
            error: "not found".to_string(),
        });

        assert_eq!(app.orch.prompts[0].status, PromptStatus::Failed);
        assert_eq!(app.orch.prompts[0].error, Some("not found".to_string()));
        assert_eq!(app.orch.active_workers, 0);
    }

    #[test]
    fn output_chunk_on_idle_transitions_to_running() {
        let mut app = app_with_prompts(&["test"]);
        app.orch.prompts[0].status = PromptStatus::Idle;

        app.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "more".to_string(),
        });

        assert_eq!(app.orch.prompts[0].status, PromptStatus::Running);
    }

    // ── select_first / select_last ──

    #[test]
    fn select_first_goes_to_zero() {
        let mut app = app_with_prompts(&["a", "b", "c", "d", "e"]);
        app.list_state.select(Some(3));

        app.select_first();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn select_last_goes_to_end() {
        let mut app = app_with_prompts(&["a", "b", "c", "d", "e"]);
        app.list_state.select(Some(1));

        app.select_last();
        assert_eq!(app.list_state.selected(), Some(4));
    }

    #[test]
    fn select_first_empty_is_noop() {
        let mut app = new_test_app();
        app.select_first();
        assert!(app.list_state.selected().is_none());
    }

    #[test]
    fn select_last_empty_is_noop() {
        let mut app = new_test_app();
        app.select_last();
        assert!(app.list_state.selected().is_none());
    }

    // ── select_half_page_down / select_half_page_up ──

    fn app_with_many_prompts(n: usize) -> App {
        let mut app = new_test_app();
        for i in 0..n {
            app.add_prompt(format!("prompt {i}"), None, false, Vec::new());
        }
        app
    }

    #[test]
    fn half_page_down_jumps_by_half_list_height() {
        let mut app = app_with_many_prompts(50);
        app.list_height = 22; // inner height = 20, half = 10
        app.list_state.select(Some(0));

        app.select_half_page_down();
        assert_eq!(app.list_state.selected(), Some(10));
    }

    #[test]
    fn half_page_up_jumps_by_half_list_height() {
        let mut app = app_with_many_prompts(50);
        app.list_height = 22;
        app.list_state.select(Some(30));

        app.select_half_page_up();
        assert_eq!(app.list_state.selected(), Some(20));
    }

    #[test]
    fn half_page_down_clamps_to_end() {
        let mut app = app_with_many_prompts(10);
        app.list_height = 22;
        app.list_state.select(Some(5));

        app.select_half_page_down();
        assert_eq!(app.list_state.selected(), Some(9));
    }

    #[test]
    fn half_page_up_clamps_to_start() {
        let mut app = app_with_many_prompts(10);
        app.list_height = 22;
        app.list_state.select(Some(3));

        app.select_half_page_up();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn half_page_empty_is_noop() {
        let mut app = new_test_app();
        app.select_half_page_down();
        app.select_half_page_up();
        assert!(app.list_state.selected().is_none());
    }

    #[test]
    fn half_page_defaults_to_10_when_no_height() {
        let mut app = app_with_many_prompts(50);
        app.list_height = 0; // not rendered yet
        app.list_state.select(Some(0));

        app.select_half_page_down();
        assert_eq!(app.list_state.selected(), Some(5));
    }

    // ── filtered navigation for new methods ──

    #[test]
    fn select_first_with_filter() {
        let mut app = app_with_prompts(&["foo", "bar", "foo2", "baz", "foo3"]);
        app.filter_text = Some("foo".to_string());
        app.rebuild_filter();
        app.list_state.select(Some(4));

        app.select_first();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn select_last_with_filter() {
        let mut app = app_with_prompts(&["foo", "bar", "foo2", "baz", "foo3"]);
        app.filter_text = Some("foo".to_string());
        app.rebuild_filter();
        app.list_state.select(Some(0));

        app.select_last();
        assert_eq!(app.list_state.selected(), Some(4));
    }

    #[test]
    fn half_page_down_with_filter() {
        let texts: Vec<&str> = (0..30).map(|i| if i % 2 == 0 { "even" } else { "odd" }).collect();
        let mut app = app_with_prompts(&texts);
        app.filter_text = Some("even".to_string());
        app.rebuild_filter();
        app.list_height = 22;
        app.list_state.select(Some(0));

        app.select_half_page_down();
        assert_eq!(app.list_state.selected(), Some(20));
    }
}
