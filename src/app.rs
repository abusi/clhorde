use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::keymap::{
    self, FilterAction, InsertAction, InteractAction, Keymap, NormalAction, ViewAction,
};
use crate::worktree;
use crate::persistence;
use crate::prompt::{Prompt, PromptMode, PromptStatus};
use crate::pty_worker::{self, PtyHandle};
use crate::worker::{WorkerInput, WorkerMessage};

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorktreeCleanup {
    Manual,
    Auto,
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
    /// Whether the quick prompts popup is visible (toggled by Ctrl+P in view mode).
    pub show_quick_prompts_popup: bool,
    /// PTY handles for interactive workers (keyed by prompt_id).
    pub pty_handles: HashMap<usize, PtyHandle>,
    /// Size of the output panel (cols, rows) from last render.
    pub output_panel_size: Option<(u16, u16)>,
    /// Last PTY size sent to workers (for change detection).
    pub last_pty_size: Option<(u16, u16)>,
    /// Maximum number of prompt files to keep on disk.
    #[allow(dead_code)]
    pub max_saved_prompts: usize,
    /// Directory for prompt persistence files (None = persistence disabled).
    pub prompts_dir: Option<PathBuf>,
    /// Whether the next submitted prompt should use a git worktree.
    pub worktree_pending: bool,
    /// Worktree cleanup policy.
    pub worktree_cleanup: WorktreeCleanup,
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
}

impl App {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(None);
        let templates = Self::load_templates();
        let history = Self::load_history();
        let settings = keymap::load_settings();
        let max_saved_prompts = settings.max_saved_prompts.unwrap_or(100);
        let list_ratio = (settings.list_ratio.unwrap_or(40) as u16).clamp(10, 90);
        let worktree_cleanup = match settings.worktree_cleanup.as_deref() {
            Some("auto") => WorktreeCleanup::Auto,
            _ => WorktreeCleanup::Manual,
        };

        let prompts_dir = persistence::default_prompts_dir();

        // Restore prompts from disk
        let mut prompts = Vec::new();
        let mut next_id: usize = 1;
        if let Some(ref dir) = prompts_dir {
            let saved = persistence::load_all_prompts(dir);
            for (uuid, pf) in &saved {
                let mode = match pf.options.mode.as_str() {
                    "one_shot" => PromptMode::OneShot,
                    _ => PromptMode::Interactive,
                };
                // All restored prompts are terminal — processes are dead
                let status = match pf.state.as_str() {
                    "failed" => PromptStatus::Failed,
                    _ => PromptStatus::Completed,
                };
                let mut prompt = Prompt::new(next_id, pf.prompt.clone(), pf.options.context.clone(), mode);
                prompt.uuid = uuid.clone();
                prompt.queue_rank = pf.queue_rank;
                prompt.session_id = pf.session_id.clone();
                prompt.worktree = pf.options.worktree.unwrap_or(false);
                prompt.worktree_path = pf.worktree_path.clone();
                prompt.tags = pf.tags.clone();
                prompt.status = status;
                prompt.seen = true;
                prompts.push(prompt);
                next_id += 1;
            }
            if !prompts.is_empty() {
                list_state.select(Some(0));
            }

            // Prune old prompt files
            persistence::prune_old_prompts(dir, max_saved_prompts);
        }

        let filtered_indices: Vec<usize> = (0..prompts.len()).collect();

        Self {
            prompts,
            next_id,
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
            filtered_indices,
            history,
            history_index: None,
            history_stash: String::new(),
            templates,
            template_suggestions: Vec::new(),
            template_suggestion_index: 0,
            show_quick_prompts_popup: false,
            pty_handles: HashMap::new(),
            output_panel_size: None,
            last_pty_size: None,
            max_saved_prompts,
            prompts_dir,
            worktree_pending: false,
            worktree_cleanup,
            list_height: 0,
            pending_g: false,
            list_ratio,
            list_collapsed: false,
            session_start: Instant::now(),
            show_help_overlay: false,
            help_scroll: 0,
            recently_moved: None,
        }
    }

    /// Save a prompt to disk if persistence is enabled.
    fn persist_prompt(&self, prompt: &Prompt) {
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(dir, &prompt.uuid, &persistence::PromptFile::from_prompt(prompt));
        }
    }

    /// Save a prompt by its ID (looks it up in self.prompts).
    fn persist_prompt_by_id(&self, prompt_id: usize) {
        if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
            self.persist_prompt(prompt);
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

    pub fn add_prompt(&mut self, text: String, cwd: Option<String>, worktree: bool, tags: Vec<String>) {
        let mut prompt = Prompt::new(self.next_id, text, cwd, self.default_mode);
        prompt.worktree = worktree;
        prompt.tags = tags;
        let max_rank = self.prompts.iter().map(|p| p.queue_rank).fold(0.0_f64, f64::max);
        prompt.queue_rank = max_rank + 1.0;
        self.next_id += 1;
        self.persist_prompt(&prompt);
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
        if let Some(prompt) = self.prompts.get(index) {
            self.persist_prompt(prompt);
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
                let mut save = false;
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    if prompt.status == PromptStatus::Running {
                        if let Some(output) = &mut prompt.output {
                            output.push('\n');
                        }
                        prompt.status = PromptStatus::Idle;
                        save = true;
                    }
                }
                if save {
                    self.persist_prompt_by_id(prompt_id);
                }
            }
            WorkerMessage::PtyUpdate { .. } => {
                // No-op: redraw happens on next loop iteration
            }
            WorkerMessage::SessionId { prompt_id, session_id } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.session_id = Some(session_id);
                }
                self.persist_prompt_by_id(prompt_id);
            }
            WorkerMessage::Finished {
                prompt_id,
                exit_code,
            } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    // For PTY workers: extract text from terminal grid before clearing state
                    if prompt.pty_state.is_some() {
                        let text = pty_worker::extract_text_from_term(
                            prompt.pty_state.as_ref().unwrap(),
                        );
                        if !text.is_empty() {
                            prompt.output = Some(text);
                        }
                        prompt.pty_state = None;
                    } else if let Some(output) = &mut prompt.output {
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
                self.persist_prompt_by_id(prompt_id);
                self.maybe_cleanup_worktree(prompt_id);
                self.pty_handles.remove(&prompt_id);
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);

                // If we're in PtyInteract for this prompt, go back to ViewOutput
                if self.mode == AppMode::PtyInteract {
                    if let Some(prompt) = self.selected_prompt() {
                        if prompt.id == prompt_id {
                            self.mode = AppMode::ViewOutput;
                        }
                    }
                }
            }
            WorkerMessage::SpawnError { prompt_id, error } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.status = PromptStatus::Failed;
                    prompt.finished_at = Some(Instant::now());
                    prompt.error = Some(error);
                    prompt.pty_state = None;
                }
                self.persist_prompt_by_id(prompt_id);
                self.maybe_cleanup_worktree(prompt_id);
                self.pty_handles.remove(&prompt_id);
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
            // Not a second g — fall through to normal handling
        }

        // Ctrl+D → half page down
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.select_half_page_down();
            self.mark_selected_seen();
            return;
        }
        // Ctrl+U → half page up
        if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.select_half_page_up();
            self.mark_selected_seen();
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
                self.worktree_pending = false;
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
                        self.list_collapsed = true;
                    }
                }
            }
            NormalAction::Interact => {
                let target_mode = self.selected_prompt().and_then(|p| {
                    if p.status == PromptStatus::Running || p.status == PromptStatus::Idle {
                        if p.pty_state.is_some() {
                            Some(AppMode::PtyInteract)
                        } else {
                            Some(AppMode::Interact)
                        }
                    } else {
                        None
                    }
                });
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
                self.select_half_page_down();
                self.mark_selected_seen();
            }
            NormalAction::HalfPageUp => {
                self.select_half_page_up();
                self.mark_selected_seen();
            }
            NormalAction::GoToTop => {
                self.select_first();
                self.mark_selected_seen();
            }
            NormalAction::GoToBottom => {
                self.select_last();
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
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
        // Ctrl+W toggles worktree mode for the current prompt
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.worktree_pending = !self.worktree_pending;
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
                    let text = self.input.trim().to_string();
                    if !text.is_empty() {
                        let (cwd, prompt_text) = Self::parse_cwd_prefix(&text);
                        if !prompt_text.is_empty() {
                            let (tags, clean_text) = crate::prompt::parse_tags(&prompt_text);
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
        // Ctrl+P toggles quick prompts popup
        if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_quick_prompts_popup = !self.show_quick_prompts_popup;
            return;
        }

        // When popup is visible, any other key closes it
        if self.show_quick_prompts_popup {
            self.show_quick_prompts_popup = false;
            // If it's Esc, consume it (don't also leave view mode)
            if key.code == KeyCode::Esc {
                return;
            }
            // Otherwise fall through to normal view mode handling (including quick prompt dispatch)
        }

        // View actions take priority
        let Some(action) = self.keymap.view.get(&key.code) else {
            // Fallback: check quick prompts
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
                let target_mode = self.selected_prompt().and_then(|p| {
                    if p.status == PromptStatus::Running || p.status == PromptStatus::Idle {
                        if p.pty_state.is_some() {
                            Some(AppMode::PtyInteract)
                        } else {
                            Some(AppMode::Interact)
                        }
                    } else {
                        None
                    }
                });
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
                    if let Some(sender) = self.worker_inputs.get(&id) {
                        let _ = sender.send(WorkerInput::Kill);
                    }
                    // Kill the child process and drop the PTY handle
                    if let Some(mut handle) = self.pty_handles.remove(&id) {
                        let _ = handle.child.kill();
                    }
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
            if let Some(sender) = self.worker_inputs.get(&id) {
                let _ = sender.send(WorkerInput::SendBytes(bytes));
            }
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

        if prompt.pty_state.is_some() {
            // PTY worker: send message as typed text + Enter (no echo needed,
            // the PTY terminal will show it)
            let mut bytes = message.as_bytes().to_vec();
            bytes.push(b'\n');
            let _ = sender.send(WorkerInput::SendBytes(bytes));
        } else {
            // Stream-json worker: echo and send as structured input
            let echo = format!("\n\n> {message}\n\n");
            match &mut prompt.output {
                Some(existing) => existing.push_str(&echo),
                None => prompt.output = Some(echo),
            }
            let mut send_text = message.clone();
            send_text.push('\n');
            let _ = sender.send(WorkerInput::SendInput(send_text));
        }
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

    fn half_page_size(&self) -> usize {
        let h = if self.list_height > 2 { self.list_height - 2 } else { 10 };
        (h as usize / 2).max(1)
    }

    fn select_half_page_down(&mut self) {
        if self.prompts.is_empty() {
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
                Some(i) => (i + step).min(self.prompts.len() - 1),
                None => 0,
            };
            self.list_state.select(Some(i));
        }
    }

    fn select_half_page_up(&mut self) {
        if self.prompts.is_empty() {
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
        if self.prompts.is_empty() {
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
        if self.prompts.is_empty() {
            return;
        }
        if self.filter_text.is_some() && !self.filtered_indices.is_empty() {
            self.list_state
                .select(Some(*self.filtered_indices.last().unwrap()));
        } else {
            self.list_state.select(Some(self.prompts.len() - 1));
        }
    }

    // ── PTY resize ──

    pub fn resize_pty_workers(&mut self, cols: u16, rows: u16) {
        for handle in self.pty_handles.values() {
            pty_worker::resize_pty(handle, cols, rows);
        }
        self.last_pty_size = Some((cols, rows));
    }

    // ── Worktree cleanup ──

    fn maybe_cleanup_worktree(&mut self, prompt_id: usize) {
        if self.worktree_cleanup != WorktreeCleanup::Auto {
            return;
        }
        let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) else {
            return;
        };
        let Some(wt_path) = prompt.worktree_path.take() else {
            return;
        };
        // Persist the cleared worktree_path
        if let Some(ref dir) = self.prompts_dir {
            if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                persistence::save_prompt(dir, &prompt.uuid, &persistence::PromptFile::from_prompt(prompt));
            }
        }
        // Spawn a background thread for cleanup to avoid blocking
        let wt_path = PathBuf::from(&wt_path);
        std::thread::spawn(move || {
            // Try to find repo root from the worktree path's parent
            if let Some(parent) = wt_path.parent() {
                // Look for the main repo among siblings
                if let Ok(entries) = std::fs::read_dir(parent) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() && path != wt_path
                            && worktree::is_git_repo(&path)
                        {
                            if let Some(root) = worktree::repo_root(&path) {
                                let _ = worktree::remove_worktree(&root, &wt_path);
                                return;
                            }
                        }
                    }
                }
            }
        });
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
        let wt = prompt.worktree;
        let tags = prompt.tags.clone();
        let mut new_prompt = Prompt::new(self.next_id, text, cwd, mode);
        new_prompt.worktree = wt;
        new_prompt.tags = tags;
        let max_rank = self.prompts.iter().map(|p| p.queue_rank).fold(0.0_f64, f64::max);
        new_prompt.queue_rank = max_rank + 1.0;
        self.next_id += 1;
        self.persist_prompt(&new_prompt);
        self.prompts.push(new_prompt);
        self.rebuild_filter();
    }

    fn resume_selected(&mut self) {
        let Some(idx) = self.list_state.selected() else {
            return;
        };
        let Some(prompt) = self.prompts.get_mut(idx) else {
            return;
        };
        if prompt.status != PromptStatus::Completed && prompt.status != PromptStatus::Failed {
            return;
        }
        // Reset the same prompt to Pending with resume flag
        prompt.status = PromptStatus::Pending;
        prompt.resume = true;
        prompt.output = None;
        prompt.error = None;
        prompt.started_at = None;
        prompt.finished_at = None;
        prompt.seen = false;
        prompt.pty_state = None;
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(dir, &self.prompts[idx].uuid, &persistence::PromptFile::from_prompt(&self.prompts[idx]));
        }
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
        // Swap queue_rank values
        let rank_a = self.prompts[idx].queue_rank;
        let rank_b = self.prompts[idx - 1].queue_rank;
        self.prompts[idx].queue_rank = rank_b;
        self.prompts[idx - 1].queue_rank = rank_a;
        self.prompts.swap(idx, idx - 1);
        // Save both to disk
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(dir, &self.prompts[idx].uuid, &persistence::PromptFile::from_prompt(&self.prompts[idx]));
            persistence::save_prompt(dir, &self.prompts[idx - 1].uuid, &persistence::PromptFile::from_prompt(&self.prompts[idx - 1]));
        }
        self.list_state.select(Some(idx - 1));
        self.recently_moved = Some((self.prompts[idx - 1].id, Instant::now()));
        self.status_message = Some((format!("Moved #{} up", self.prompts[idx - 1].id), Instant::now()));
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
        // Swap queue_rank values
        let rank_a = self.prompts[idx].queue_rank;
        let rank_b = self.prompts[idx + 1].queue_rank;
        self.prompts[idx].queue_rank = rank_b;
        self.prompts[idx + 1].queue_rank = rank_a;
        self.prompts.swap(idx, idx + 1);
        // Save both to disk
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(dir, &self.prompts[idx].uuid, &persistence::PromptFile::from_prompt(&self.prompts[idx]));
            persistence::save_prompt(dir, &self.prompts[idx + 1].uuid, &persistence::PromptFile::from_prompt(&self.prompts[idx + 1]));
        }
        self.list_state.select(Some(idx + 1));
        self.recently_moved = Some((self.prompts[idx + 1].id, Instant::now()));
        self.status_message = Some((format!("Moved #{} down", self.prompts[idx + 1].id), Instant::now()));
        self.rebuild_filter();
    }

    // ── Feature 5: Filter ──

    fn rebuild_filter(&mut self) {
        self.filtered_indices = match &self.filter_text {
            Some(filter) => {
                // Split filter into @tag tokens and text tokens
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

                self.prompts
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        // All @tag filters must match
                        let tags_match = tag_filters.iter().all(|tf| {
                            p.tags.iter().any(|t| t.to_lowercase() == *tf)
                        });
                        // Text filter must match prompt text (if present)
                        let text_match = text_filter.is_empty()
                            || p.text.to_lowercase().contains(&text_filter);
                        tags_match && text_match
                    })
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

    /// Test-only constructor that skips persistence loading.
    fn new_test_app() -> App {
        let mut list_state = ListState::default();
        list_state.select(None);
        App {
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
            keymap: Keymap::default(),
            status_message: None,
            confirm_quit: false,
            filter_text: None,
            filter_input: String::new(),
            filtered_indices: Vec::new(),
            history: Vec::new(),
            history_index: None,
            history_stash: String::new(),
            templates: HashMap::new(),
            template_suggestions: Vec::new(),
            template_suggestion_index: 0,
            show_quick_prompts_popup: false,
            pty_handles: HashMap::new(),
            output_panel_size: None,
            last_pty_size: None,
            max_saved_prompts: 100,
            prompts_dir: None,
            worktree_pending: false,
            worktree_cleanup: WorktreeCleanup::Manual,
            list_height: 0,
            pending_g: false,
            list_ratio: 40,
            list_collapsed: false,
            session_start: Instant::now(),
            show_help_overlay: false,
            help_scroll: 0,
            recently_moved: None,
        }
    }

    // ── App::new defaults ──

    #[test]
    fn app_new_defaults() {
        let app = new_test_app();
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
        let mut app = new_test_app();
        app.add_prompt("first".to_string(), None, false, Vec::new());
        app.add_prompt("second".to_string(), None, false, Vec::new());
        app.add_prompt("third".to_string(), None, false, Vec::new());

        assert_eq!(app.prompts.len(), 3);
        assert_eq!(app.prompts[0].id, 1);
        assert_eq!(app.prompts[1].id, 2);
        assert_eq!(app.prompts[2].id, 3);
        assert_eq!(app.next_id, 4);
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

        app.prompts[0].status = PromptStatus::Completed;
        app.prompts[1].status = PromptStatus::Failed;

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
        assert_eq!(app.filtered_indices, vec![0]); // only first has both tags
    }

    // ── history_prev / history_next ──

    #[test]
    fn history_empty_is_noop() {
        let mut app = new_test_app();
        app.history.clear();
        app.input = "current".to_string();
        app.history_prev();
        assert_eq!(app.input, "current");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn history_prev_stashes_and_navigates() {
        let mut app = new_test_app();
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
        let mut app = new_test_app();
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
        let mut app = new_test_app();
        app.history = vec!["first".to_string(), "second".to_string()];
        app.input = "typing".to_string();

        app.history_prev(); // "second"
        app.history_next(); // past end -> restore stash
        assert_eq!(app.input, "typing");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn history_next_without_navigating_is_noop() {
        let mut app = new_test_app();
        app.history = vec!["first".to_string()];
        app.input = "current".to_string();
        app.history_next();
        assert_eq!(app.input, "current");
    }

    #[test]
    fn history_prev_next_roundtrip() {
        let mut app = new_test_app();
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
        // Default fallback: height=10, half=5
        assert_eq!(app.list_state.selected(), Some(5));
    }

    // ── filtered navigation for new methods ──

    #[test]
    fn select_first_with_filter() {
        let mut app = app_with_prompts(&["foo", "bar", "foo2", "baz", "foo3"]);
        app.filter_text = Some("foo".to_string());
        app.rebuild_filter();
        // filtered_indices = [0, 2, 4]
        app.list_state.select(Some(4)); // last filtered item

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
        // filtered_indices = [0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28]
        app.list_height = 22; // half = 10
        app.list_state.select(Some(0)); // filter pos 0

        app.select_half_page_down();
        // jump 10 positions in filtered list: pos 10 → index 20
        assert_eq!(app.list_state.selected(), Some(20));
    }
}
