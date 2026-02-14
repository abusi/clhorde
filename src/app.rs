use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::prompt::{Prompt, PromptStatus};
use crate::worker::{WorkerInput, WorkerMessage};

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Insert,
    ViewOutput,
    Interact,
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
}

impl App {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(None);
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
        let prompt = Prompt::new(self.next_id, text, cwd);
        self.next_id += 1;
        self.prompts.push(prompt);
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
            self.input = format!("{}/", path);
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
                        prompt.status = PromptStatus::Idle;
                    }
                }
            }
            WorkerMessage::Finished {
                prompt_id,
                exit_code,
            } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
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

    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            AppMode::Normal => self.handle_normal_key(key),
            AppMode::Insert => self.handle_insert_key(key),
            AppMode::ViewOutput => self.handle_view_key(key),
            AppMode::Interact => self.handle_interact_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('i') => {
                self.mode = AppMode::Insert;
                self.input.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                self.mark_selected_seen();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                self.mark_selected_seen();
            }
            KeyCode::Enter => {
                if let Some(idx) = self.list_state.selected() {
                    if idx < self.prompts.len() {
                        self.prompts[idx].seen = true;
                        self.scroll_offset = 0;
                        self.mode = AppMode::ViewOutput;
                    }
                }
            }
            KeyCode::Char('s') => {
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
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.max_workers = (self.max_workers + 1).min(20);
            }
            KeyCode::Char('-') => {
                self.max_workers = self.max_workers.saturating_sub(1).max(1);
            }
            _ => {}
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.input.clear();
                self.suggestions.clear();
                self.suggestion_index = 0;
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if !text.is_empty() {
                    let (cwd, prompt_text) = Self::parse_cwd_prefix(&text);
                    if !prompt_text.is_empty() {
                        self.add_prompt(prompt_text, cwd);
                    }
                }
                self.input.clear();
                self.suggestions.clear();
                self.suggestion_index = 0;
                self.mode = AppMode::Normal;
            }
            KeyCode::Tab => {
                self.accept_suggestion();
            }
            KeyCode::Down => {
                if !self.suggestions.is_empty() {
                    self.suggestion_index = (self.suggestion_index + 1) % self.suggestions.len();
                }
            }
            KeyCode::Up => {
                if !self.suggestions.is_empty() {
                    self.suggestion_index = if self.suggestion_index == 0 {
                        self.suggestions.len() - 1
                    } else {
                        self.suggestion_index - 1
                    };
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.update_suggestions();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.update_suggestions();
            }
            _ => {}
        }
    }

    fn handle_view_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Normal;
                self.scroll_offset = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::Char('s') => {
                if let Some(prompt) = self.selected_prompt() {
                    if prompt.status == PromptStatus::Running
                        || prompt.status == PromptStatus::Idle
                    {
                        self.interact_input.clear();
                        self.mode = AppMode::Interact;
                    }
                }
            }
            KeyCode::Char('f') => {
                self.auto_scroll = !self.auto_scroll;
            }
            KeyCode::Char('x') => {
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
            _ => {}
        }
    }

    fn handle_interact_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
                self.interact_input.clear();
            }
            KeyCode::Enter => {
                if let Some(idx) = self.list_state.selected() {
                    if let Some(prompt) = self.prompts.get_mut(idx) {
                        let id = prompt.id;
                        if let Some(sender) = self.worker_inputs.get(&id) {
                            let text = self.interact_input.clone();
                            // Echo user input into the output panel
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
            KeyCode::Backspace => {
                self.interact_input.pop();
            }
            KeyCode::Char(c) => {
                self.interact_input.push(c);
            }
            _ => {}
        }
    }

    fn select_next(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => (i + 1).min(self.prompts.len() - 1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }
}
