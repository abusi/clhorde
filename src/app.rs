use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::keymap::{
    FilterAction, InsertAction, InteractAction, Keymap, NormalAction, ViewAction,
};
use crate::prompt::{Prompt, PromptMode, PromptStatus, SerializablePrompt};
use crate::worker::{WorkerInput, WorkerMessage};

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Insert,
    ViewOutput,
    Interact,
    Filter,
}

pub struct App {
    pub prompts: Vec<Prompt>,
    pub next_id: usize,
    pub max_workers: usize,
    pub active_workers: usize,
    pub mode: AppMode,
    pub list_state: ListState,
    pub input: String,
    pub scroll_offset: u16,
    pub should_quit: bool,
    pub worker_inputs: HashMap<usize, mpsc::UnboundedSender<WorkerInput>>,
    pub interact_input: String,
    pub auto_scroll: bool,
    pub suggestions: Vec<String>,
    pub suggestion_index: usize,
    /// Tick counter incremented every 100ms, used for pulsing animations.
    pub tick: u64,
    pub default_mode: PromptMode,
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
    pub templates: HashMap<String, String>,
    /// Template suggestion names matching current input.
    pub template_suggestions: Vec<String>,
    /// Selected template suggestion index.
    pub template_suggestion_index: usize,
}

impl App {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(None);
        let templates = Self::load_templates();
        let history = Self::load_history();
        Self {
            prompts: Vec::new(),
            next_id: 1,
            max_workers: 3,
            active_workers: 0,
            mode: AppMode::Normal,
            list_state,
            input: String::new(),
            scroll_offset: 0,
            should_quit: false,
            worker_inputs: HashMap::new(),
            interact_input: String::new(),
            auto_scroll: true,
            suggestions: Vec::new(),
            suggestion_index: 0,
            tick: 0,
            default_mode: PromptMode::Interactive,
            keymap: Keymap::load(),
            status_message: None,
            confirm_quit: false,
            filter_text: None,
            filter_input: String::new(),
            filtered_indices: Vec::new(),
            history,
            history_index: None,
            history_stash: String::new(),
            templates,
            template_suggestions: Vec::new(),
            template_suggestion_index: 0,
        }
    }

    pub fn pending_count(&self) -> usize {
        self.prompts
            .iter()
            .filter(|p| p.status == PromptStatus::Pending)
            .count()
    }

    pub fn completed_count(&self) -> usize {
        self.prompts
            .iter()
            .filter(|p| p.status == PromptStatus::Completed || p.status == PromptStatus::Failed)
            .count()
    }

    pub fn add_prompt(&mut self, text: String, cwd: Option<String>) {
        let prompt = Prompt::new(self.next_id, text, cwd, self.default_mode);
        self.next_id += 1;
        self.prompts.push(prompt);
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

        // Don't suggest if `: ` already present (user is typing the prompt text)
        if self.input.contains(": ") {
            return;
        }

        let input = self.input.trim();
        if input.is_empty() {
            return;
        }

        let path = Path::new(input);

        // If input ends with `/` and is a directory, list its children
        // Otherwise split into parent + partial filename prefix
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
        if let Some(path) = self.suggestions.get(self.suggestion_index) {
            self.input = format!("{path}/");
            self.suggestions.clear();
            self.suggestion_index = 0;
            self.update_suggestions();
        }
    }

    pub fn next_pending_prompt_index(&self) -> Option<usize> {
        self.prompts
            .iter()
            .position(|p| p.status == PromptStatus::Pending)
    }

    pub fn mark_running(&mut self, index: usize) {
        if let Some(prompt) = self.prompts.get_mut(index) {
            prompt.status = PromptStatus::Running;
            prompt.started_at = Some(Instant::now());
        }
    }

    pub fn apply_message(&mut self, msg: WorkerMessage) {
        match msg {
            WorkerMessage::OutputChunk { prompt_id, text } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    // If we get output after being idle, we're running again
                    if prompt.status == PromptStatus::Idle {
                        prompt.status = PromptStatus::Running;
                    }
                    match &mut prompt.output {
                        Some(existing) => existing.push_str(&text),
                        None => prompt.output = Some(text),
                    }
                }
            }
            WorkerMessage::TurnComplete { prompt_id } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    if prompt.status == PromptStatus::Running {
                        if let Some(output) = &mut prompt.output {
                            output.push('\n');
                        }
                        prompt.status = PromptStatus::Idle;
                    }
                }
            }
            WorkerMessage::Finished {
                prompt_id,
                exit_code,
            } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    if let Some(output) = &mut prompt.output {
                        output.push('\n');
                    }
                    prompt.finished_at = Some(Instant::now());
                    if exit_code == Some(0) || exit_code.is_none() {
                        prompt.status = PromptStatus::Completed;
                    } else {
                        prompt.status = PromptStatus::Failed;
                        if prompt.error.is_none() {
                            prompt.error = Some(format!("Exit code: {}", exit_code.unwrap()));
                        }
                    }
                }
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);
            }
            WorkerMessage::SpawnError { prompt_id, error } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.status = PromptStatus::Failed;
                    prompt.finished_at = Some(Instant::now());
                    prompt.error = Some(error);
                }
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);
            }
        }
    }

    pub fn selected_prompt(&self) -> Option<&Prompt> {
        self.list_state
            .selected()
            .and_then(|i| self.prompts.get(i))
    }

    /// Mark the currently selected prompt as seen if it's finished.
    fn mark_selected_seen(&mut self) {
        if let Some(idx) = self.list_state.selected() {
            if let Some(prompt) = self.prompts.get_mut(idx) {
                if prompt.status == PromptStatus::Completed || prompt.status == PromptStatus::Failed
                {
                    prompt.seen = true;
                }
            }
        }
    }

    /// Clear expired status messages (older than 3 seconds).
    pub fn clear_expired_status(&mut self) {
        if let Some((_, created)) = &self.status_message {
            if created.elapsed().as_secs() >= 3 {
                self.status_message = None;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Quit confirmation intercepts all keys
        if self.confirm_quit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.should_quit = true,
                _ => self.confirm_quit = false,
            }
            return;
        }

        match self.mode {
            AppMode::Normal => self.handle_normal_key(key),
            AppMode::Insert => self.handle_insert_key(key),
            AppMode::ViewOutput => self.handle_view_key(key),
            AppMode::Interact => self.handle_interact_key(key),
            AppMode::Filter => self.handle_filter_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        let Some(action) = self.keymap.normal.get(&key.code) else {
            return;
        };
        match action {
            NormalAction::Quit => {
                let has_active = self.prompts.iter().any(|p| {
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
                self.history_index = None;
                self.history_stash.clear();
                self.template_suggestions.clear();
                self.template_suggestion_index = 0;
            }
            NormalAction::SelectNext => {
                self.select_next();
                self.mark_selected_seen();
            }
            NormalAction::SelectPrev => {
                self.select_prev();
                self.mark_selected_seen();
            }
            NormalAction::ViewOutput => {
                if let Some(idx) = self.list_state.selected() {
                    if idx < self.prompts.len() {
                        self.prompts[idx].seen = true;
                        self.scroll_offset = 0;
                        self.mode = AppMode::ViewOutput;
                    }
                }
            }
            NormalAction::Interact => {
                if let Some(prompt) = self.selected_prompt() {
                    if prompt.status == PromptStatus::Running
                        || prompt.status == PromptStatus::Idle
                    {
                        self.interact_input.clear();
                        self.scroll_offset = 0;
                        self.mode = AppMode::Interact;
                    }
                }
            }
            NormalAction::IncreaseWorkers => {
                self.max_workers = (self.max_workers + 1).min(20);
            }
            NormalAction::DecreaseWorkers => {
                self.max_workers = self.max_workers.saturating_sub(1).max(1);
            }
            NormalAction::ToggleMode => {
                self.default_mode = self.default_mode.toggle();
            }
            NormalAction::Retry => {
                self.retry_selected();
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
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
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
                }
                InsertAction::Submit => {
                    let text = self.input.trim().to_string();
                    if !text.is_empty() {
                        let (cwd, prompt_text) = Self::parse_cwd_prefix(&text);
                        if !prompt_text.is_empty() {
                            self.add_prompt(prompt_text.clone(), cwd);
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
                self.input.pop();
                self.history_index = None;
                self.update_suggestions();
                self.update_template_suggestions();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.history_index = None;
                self.update_suggestions();
                self.update_template_suggestions();
            }
            _ => {}
        }
    }

    fn handle_view_key(&mut self, key: KeyEvent) {
        // View actions take priority
        let Some(action) = self.keymap.view.get(&key.code) else {
            // Fallback: check quick prompts
            self.try_quick_prompt(&key);
            return;
        };
        match action {
            ViewAction::Back => {
                self.mode = AppMode::Normal;
                self.scroll_offset = 0;
            }
            ViewAction::ScrollDown => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            ViewAction::ScrollUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            ViewAction::Interact => {
                if let Some(prompt) = self.selected_prompt() {
                    if prompt.status == PromptStatus::Running
                        || prompt.status == PromptStatus::Idle
                    {
                        self.interact_input.clear();
                        self.mode = AppMode::Interact;
                    }
                }
            }
            ViewAction::ToggleAutoscroll => {
                self.auto_scroll = !self.auto_scroll;
            }
            ViewAction::KillWorker => {
                if let Some(prompt) = self.selected_prompt() {
                    let id = prompt.id;
                    if prompt.status == PromptStatus::Running
                        || prompt.status == PromptStatus::Idle
                    {
                        if let Some(sender) = self.worker_inputs.get(&id) {
                            let _ = sender.send(WorkerInput::Kill);
                        }
                    }
                }
            }
            ViewAction::Export => {
                self.export_selected_output();
            }
        }
    }

    fn handle_interact_key(&mut self, key: KeyEvent) {
        if let Some(action) = self.keymap.interact.get(&key.code) {
            match action {
                InteractAction::Back => {
                    self.mode = AppMode::Normal;
                    self.interact_input.clear();
                }
                InteractAction::Send => {
                    if let Some(idx) = self.list_state.selected() {
                        if let Some(prompt) = self.prompts.get_mut(idx) {
                            let id = prompt.id;
                            if let Some(sender) = self.worker_inputs.get(&id) {
                                let text = self.interact_input.clone();
                                let echo = format!("\n\n> {text}\n\n");
                                match &mut prompt.output {
                                    Some(existing) => existing.push_str(&echo),
                                    None => prompt.output = Some(echo),
                                }
                                let mut send_text = text;
                                send_text.push('\n');
                                let _ = sender.send(WorkerInput::SendInput(send_text));
                            }
                        }
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
                    // Adjust selection to be valid within filtered view
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
                // Live filter as user types
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

    fn try_quick_prompt(&mut self, key: &KeyEvent) {
        let Some(message) = self.keymap.quick_prompts.get(&key.code) else {
            return;
        };
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let Some(prompt) = self.prompts.get_mut(idx) else {
            return;
        };
        if prompt.status != PromptStatus::Running && prompt.status != PromptStatus::Idle {
            return;
        }
        let id = prompt.id;
        let Some(sender) = self.worker_inputs.get(&id) else {
            return;
        };
        let echo = format!("\n\n> {message}\n\n");
        match &mut prompt.output {
            Some(existing) => existing.push_str(&echo),
            None => prompt.output = Some(echo),
        }
        let mut send_text = message.clone();
        send_text.push('\n');
        let _ = sender.send(WorkerInput::SendInput(send_text));
    }

    fn select_next(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            // Navigate within filtered list
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
                Some(i) => (i + 1).min(self.prompts.len() - 1),
                None => 0,
            };
            self.list_state.select(Some(i));
        }
    }

    fn select_prev(&mut self) {
        if self.prompts.is_empty() {
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
        let new_prompt = Prompt::new(self.next_id, text, cwd, mode);
        self.next_id += 1;
        self.prompts.push(new_prompt);
        self.rebuild_filter();
    }

    // ── Feature 4: Reorder ──

    fn move_selected_up(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        if idx == 0 {
            return;
        }
        // Only move pending prompts
        if self.prompts[idx].status != PromptStatus::Pending {
            return;
        }
        self.prompts.swap(idx, idx - 1);
        self.list_state.select(Some(idx - 1));
        self.rebuild_filter();
    }

    fn move_selected_down(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        if idx >= self.prompts.len() - 1 {
            return;
        }
        if self.prompts[idx].status != PromptStatus::Pending {
            return;
        }
        self.prompts.swap(idx, idx + 1);
        self.list_state.select(Some(idx + 1));
        self.rebuild_filter();
    }

    // ── Feature 5: Filter ──

    fn rebuild_filter(&mut self) {
        self.filtered_indices = match &self.filter_text {
            Some(filter) => {
                let lower = filter.to_lowercase();
                self.prompts
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| p.text.to_lowercase().contains(&lower))
                    .map(|(i, _)| i)
                    .collect()
            }
            None => (0..self.prompts.len()).collect(),
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

    // ── Feature 6: History ──

    fn data_dir() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("clhorde"))
    }

    fn history_path() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("history"))
    }

    fn load_history() -> Vec<String> {
        let Some(path) = Self::history_path() else {
            return Vec::new();
        };
        match fs::read_to_string(&path) {
            Ok(content) => content
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn append_history(&mut self, text: &str) {
        // Avoid duplicating the most recent entry
        if self.history.last().map(|s| s.as_str()) == Some(text) {
            return;
        }
        self.history.push(text.to_string());

        // Persist to file
        if let Some(path) = Self::history_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(file, "{text}");
            }
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                // Start navigating: stash current input
                self.history_stash = self.input.clone();
                let idx = self.history.len() - 1;
                self.history_index = Some(idx);
                self.input = self.history[idx].clone();
            }
            Some(idx) => {
                if idx > 0 {
                    let new_idx = idx - 1;
                    self.history_index = Some(new_idx);
                    self.input = self.history[new_idx].clone();
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
            self.input = self.history[new_idx].clone();
        } else {
            // Past the end: restore stashed input
            self.history_index = None;
            self.input = self.history_stash.clone();
            self.history_stash.clear();
        }
    }

    // ── Feature 7: Session persistence ──

    fn session_path() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("session.json"))
    }

    pub fn save_session(&self) {
        let Some(path) = Self::session_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let serializable: Vec<SerializablePrompt> =
            self.prompts.iter().map(SerializablePrompt::from).collect();
        if let Ok(json) = serde_json::to_string_pretty(&serializable) {
            let _ = fs::write(&path, json);
        }
    }

    pub fn load_session(&mut self) {
        let Some(path) = Self::session_path() else {
            return;
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return,
        };
        let serialized: Vec<SerializablePrompt> = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return,
        };

        for sp in serialized {
            let prompt = sp.into_prompt();
            if prompt.id >= self.next_id {
                self.next_id = prompt.id + 1;
            }
            self.prompts.push(prompt);
        }

        self.rebuild_filter();
        if !self.prompts.is_empty() && self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }
    }

    // ── Feature 8: Templates ──

    fn templates_path() -> Option<PathBuf> {
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(config_dir.join("clhorde").join("templates.toml"))
    }

    fn load_templates() -> HashMap<String, String> {
        let Some(path) = Self::templates_path() else {
            return HashMap::new();
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        #[derive(serde::Deserialize)]
        struct TemplateConfig {
            templates: Option<HashMap<String, String>>,
        }

        match toml::from_str::<TemplateConfig>(&content) {
            Ok(config) => config.templates.unwrap_or_default(),
            Err(_) => {
                // Try as flat key-value pairs (no [templates] section)
                toml::from_str::<HashMap<String, String>>(&content).unwrap_or_default()
            }
        }
    }

    fn update_template_suggestions(&mut self) {
        self.template_suggestions.clear();
        self.template_suggestion_index = 0;

        if self.templates.is_empty() {
            return;
        }

        // Check if input starts with `:` and has no space yet (still typing template name)
        let input = &self.input;
        if !input.starts_with(':') {
            return;
        }

        let prefix = &input[1..]; // after the colon
        if prefix.contains(' ') {
            return; // already expanded or typing after template
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
                self.input = format!("{template_text} ");
                self.template_suggestions.clear();
                self.template_suggestion_index = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::WorkerMessage;

    // ── App::new defaults ──

    #[test]
    fn app_new_defaults() {
        let app = App::new();
        assert_eq!(app.max_workers, 3);
        assert_eq!(app.active_workers, 0);
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.auto_scroll);
        assert!(app.prompts.is_empty());
        assert_eq!(app.next_id, 1);
        assert!(!app.should_quit);
        assert!(!app.confirm_quit);
        assert_eq!(app.default_mode, PromptMode::Interactive);
        assert!(app.filter_text.is_none());
        assert!(app.history_index.is_none());
    }

    // ── add_prompt / pending_count / completed_count ──

    #[test]
    fn add_prompt_increments_id() {
        let mut app = App::new();
        app.add_prompt("first".to_string(), None);
        app.add_prompt("second".to_string(), None);
        app.add_prompt("third".to_string(), None);

        assert_eq!(app.prompts.len(), 3);
        assert_eq!(app.prompts[0].id, 1);
        assert_eq!(app.prompts[1].id, 2);
        assert_eq!(app.prompts[2].id, 3);
        assert_eq!(app.next_id, 4);
    }

    #[test]
    fn add_prompt_selects_first() {
        let mut app = App::new();
        assert!(app.list_state.selected().is_none());
        app.add_prompt("test".to_string(), None);
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn pending_and_completed_counts() {
        let mut app = App::new();
        app.add_prompt("a".to_string(), None);
        app.add_prompt("b".to_string(), None);
        app.add_prompt("c".to_string(), None);

        assert_eq!(app.pending_count(), 3);
        assert_eq!(app.completed_count(), 0);

        app.prompts[0].status = PromptStatus::Completed;
        app.prompts[1].status = PromptStatus::Failed;

        assert_eq!(app.pending_count(), 1);
        assert_eq!(app.completed_count(), 2);
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_empty_list() {
        let mut app = App::new();
        app.select_next(); // should not panic
        assert!(app.list_state.selected().is_none());
    }

    #[test]
    fn select_prev_empty_list() {
        let mut app = App::new();
        app.select_prev(); // should not panic
        assert!(app.list_state.selected().is_none());
    }

    #[test]
    fn select_next_clamps_to_end() {
        let mut app = App::new();
        app.add_prompt("a".to_string(), None);
        app.add_prompt("b".to_string(), None);
        app.list_state.select(Some(1));

        app.select_next();
        assert_eq!(app.list_state.selected(), Some(1)); // stays at end
    }

    #[test]
    fn select_prev_clamps_to_start() {
        let mut app = App::new();
        app.add_prompt("a".to_string(), None);
        app.add_prompt("b".to_string(), None);
        app.list_state.select(Some(0));

        app.select_prev();
        assert_eq!(app.list_state.selected(), Some(0)); // stays at start
    }

    #[test]
    fn select_next_advances() {
        let mut app = App::new();
        app.add_prompt("a".to_string(), None);
        app.add_prompt("b".to_string(), None);
        app.add_prompt("c".to_string(), None);
        app.list_state.select(Some(0));

        app.select_next();
        assert_eq!(app.list_state.selected(), Some(1));
        app.select_next();
        assert_eq!(app.list_state.selected(), Some(2));
    }

    #[test]
    fn select_prev_goes_back() {
        let mut app = App::new();
        app.add_prompt("a".to_string(), None);
        app.add_prompt("b".to_string(), None);
        app.list_state.select(Some(1));

        app.select_prev();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    // ── move_selected_up / move_selected_down ──

    fn app_with_prompts(texts: &[&str]) -> App {
        let mut app = App::new();
        for t in texts {
            app.add_prompt(t.to_string(), None);
        }
        app
    }

    #[test]
    fn move_down_swaps_pending() {
        let mut app = app_with_prompts(&["a", "b", "c"]);
        app.list_state.select(Some(0));

        app.move_selected_down();
        assert_eq!(app.prompts[0].text, "b");
        assert_eq!(app.prompts[1].text, "a");
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn move_up_swaps_pending() {
        let mut app = app_with_prompts(&["a", "b", "c"]);
        app.list_state.select(Some(2));

        app.move_selected_up();
        assert_eq!(app.prompts[1].text, "c");
        assert_eq!(app.prompts[2].text, "b");
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn move_down_at_end_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.list_state.select(Some(1));

        app.move_selected_down();
        assert_eq!(app.prompts[0].text, "a");
        assert_eq!(app.prompts[1].text, "b");
        assert_eq!(app.list_state.selected(), Some(1));
    }

    #[test]
    fn move_up_at_start_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.list_state.select(Some(0));

        app.move_selected_up();
        assert_eq!(app.prompts[0].text, "a");
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn move_non_pending_is_noop() {
        let mut app = app_with_prompts(&["a", "b"]);
        app.prompts[0].status = PromptStatus::Running;
        app.list_state.select(Some(0));

        app.move_selected_down();
        assert_eq!(app.prompts[0].text, "a");
        assert_eq!(app.prompts[1].text, "b");
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
        app.prompts[0].status = PromptStatus::Completed;
        app.prompts[0].cwd = Some("/tmp".to_string());
        app.list_state.select(Some(0));

        app.retry_selected();

        assert_eq!(app.prompts.len(), 2);
        let retried = &app.prompts[1];
        assert_eq!(retried.text, "hello world");
        assert_eq!(retried.cwd, Some("/tmp".to_string()));
        assert_eq!(retried.status, PromptStatus::Pending);
        assert!(retried.id > app.prompts[0].id);
    }

    #[test]
    fn retry_failed_creates_new_prompt() {
        let mut app = app_with_prompts(&["fail"]);
        app.prompts[0].status = PromptStatus::Failed;
        app.list_state.select(Some(0));

        app.retry_selected();
        assert_eq!(app.prompts.len(), 2);
        assert_eq!(app.prompts[1].status, PromptStatus::Pending);
    }

    #[test]
    fn retry_running_is_noop() {
        let mut app = app_with_prompts(&["running"]);
        app.prompts[0].status = PromptStatus::Running;
        app.list_state.select(Some(0));

        app.retry_selected();
        assert_eq!(app.prompts.len(), 1);
    }

    #[test]
    fn retry_pending_is_noop() {
        let mut app = app_with_prompts(&["pending"]);
        app.list_state.select(Some(0));

        app.retry_selected();
        assert_eq!(app.prompts.len(), 1);
    }

    #[test]
    fn retry_no_selection_is_noop() {
        let mut app = app_with_prompts(&["test"]);
        app.prompts[0].status = PromptStatus::Completed;
        app.list_state.select(None);

        app.retry_selected();
        assert_eq!(app.prompts.len(), 1);
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
        // "bar" is filtered out, selection should snap to first match
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

    // ── history_prev / history_next ──

    #[test]
    fn history_empty_is_noop() {
        let mut app = App::new();
        app.input = "current".to_string();
        app.history_prev();
        assert_eq!(app.input, "current");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn history_prev_stashes_and_navigates() {
        let mut app = App::new();
        app.history = vec!["first".to_string(), "second".to_string()];
        app.input = "typing".to_string();

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
        let mut app = App::new();
        app.history = vec!["only".to_string()];
        app.input = "typing".to_string();

        app.history_prev();
        assert_eq!(app.input, "only");
        assert_eq!(app.history_index, Some(0));

        app.history_prev(); // already at start
        assert_eq!(app.input, "only");
        assert_eq!(app.history_index, Some(0));
    }

    #[test]
    fn history_next_restores_stash() {
        let mut app = App::new();
        app.history = vec!["first".to_string(), "second".to_string()];
        app.input = "typing".to_string();

        app.history_prev(); // "second"
        app.history_next(); // past end -> restore stash
        assert_eq!(app.input, "typing");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn history_next_without_navigating_is_noop() {
        let mut app = App::new();
        app.history = vec!["first".to_string()];
        app.input = "current".to_string();
        app.history_next();
        assert_eq!(app.input, "current");
    }

    #[test]
    fn history_prev_next_roundtrip() {
        let mut app = App::new();
        app.history = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        app.input = "now".to_string();

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
        // /tmp should exist on any unix system
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
        app.prompts[0].status = PromptStatus::Running;

        app.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "hello ".to_string(),
        });
        app.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "world".to_string(),
        });

        assert_eq!(app.prompts[0].output, Some("hello world".to_string()));
    }

    #[test]
    fn apply_turn_complete_transitions_to_idle() {
        let mut app = app_with_prompts(&["test"]);
        app.prompts[0].status = PromptStatus::Running;
        app.prompts[0].output = Some("output".to_string());

        app.apply_message(WorkerMessage::TurnComplete { prompt_id: 1 });

        assert_eq!(app.prompts[0].status, PromptStatus::Idle);
        assert_eq!(app.prompts[0].output, Some("output\n".to_string()));
    }

    #[test]
    fn apply_finished_success() {
        let mut app = app_with_prompts(&["test"]);
        app.prompts[0].status = PromptStatus::Running;
        app.active_workers = 1;

        app.apply_message(WorkerMessage::Finished {
            prompt_id: 1,
            exit_code: Some(0),
        });

        assert_eq!(app.prompts[0].status, PromptStatus::Completed);
        assert!(app.prompts[0].finished_at.is_some());
        assert_eq!(app.active_workers, 0);
    }

    #[test]
    fn apply_finished_failure() {
        let mut app = app_with_prompts(&["test"]);
        app.prompts[0].status = PromptStatus::Running;
        app.active_workers = 1;

        app.apply_message(WorkerMessage::Finished {
            prompt_id: 1,
            exit_code: Some(1),
        });

        assert_eq!(app.prompts[0].status, PromptStatus::Failed);
        assert!(app.prompts[0].error.is_some());
    }

    #[test]
    fn apply_spawn_error() {
        let mut app = app_with_prompts(&["test"]);
        app.active_workers = 1;

        app.apply_message(WorkerMessage::SpawnError {
            prompt_id: 1,
            error: "not found".to_string(),
        });

        assert_eq!(app.prompts[0].status, PromptStatus::Failed);
        assert_eq!(app.prompts[0].error, Some("not found".to_string()));
        assert_eq!(app.active_workers, 0);
    }

    #[test]
    fn output_chunk_on_idle_transitions_to_running() {
        let mut app = app_with_prompts(&["test"]);
        app.prompts[0].status = PromptStatus::Idle;

        app.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "more".to_string(),
        });

        assert_eq!(app.prompts[0].status, PromptStatus::Running);
    }
}
