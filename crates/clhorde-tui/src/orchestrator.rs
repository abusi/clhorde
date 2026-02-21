use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::keymap;
use crate::pty_worker::{self, PtyHandle};
use crate::worker::{WorkerInput, WorkerMessage};
use clhorde_core::persistence;
use clhorde_core::prompt::{Prompt, PromptMode, PromptStatus};

use clhorde_core::config::WorktreeCleanup;

/// Effect returned by `apply_message` so the caller (App) can react to
/// state changes that require TUI-level decisions.
pub enum MsgEffect {
    None,
    /// A prompt finished — the App should check if it needs to leave PtyInteract.
    PromptFinished { prompt_id: usize },
}

/// Result of a move operation.
pub enum MoveResult {
    Moved { prompt_id: usize },
    NotMoved,
}

/// Orchestration state: prompts, workers, persistence.
///
/// This struct contains everything needed to manage the prompt lifecycle
/// independently of the TUI. The TUI (App) owns an Orchestrator and
/// delegates prompt/worker mutations to it.
pub struct Orchestrator {
    pub prompts: Vec<Prompt>,
    pub next_id: usize,
    pub max_workers: usize,
    pub active_workers: usize,
    pub worker_inputs: HashMap<usize, mpsc::UnboundedSender<WorkerInput>>,
    pub pty_handles: HashMap<usize, PtyHandle>,
    pub default_mode: PromptMode,
    #[allow(dead_code)]
    pub max_saved_prompts: usize,
    pub prompts_dir: Option<PathBuf>,
    pub worktree_cleanup: WorktreeCleanup,
}

impl Orchestrator {
    pub fn new() -> Self {
        let settings = keymap::load_settings();
        let max_saved_prompts = settings.max_saved_prompts.unwrap_or(100);
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
                let status = match pf.state.as_str() {
                    "failed" => PromptStatus::Failed,
                    _ => PromptStatus::Completed,
                };
                let mut prompt =
                    Prompt::new(next_id, pf.prompt.clone(), pf.options.context.clone(), mode);
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

            persistence::prune_old_prompts(dir, max_saved_prompts);
        }

        Self {
            prompts,
            next_id,
            max_workers: 3,
            active_workers: 0,
            worker_inputs: HashMap::new(),
            pty_handles: HashMap::new(),
            default_mode: PromptMode::Interactive,
            max_saved_prompts,
            prompts_dir,
            worktree_cleanup,
        }
    }

    /// Test-only constructor that skips persistence loading.
    #[cfg(test)]
    pub fn new_test() -> Self {
        Self {
            prompts: Vec::new(),
            next_id: 1,
            max_workers: 3,
            active_workers: 0,
            worker_inputs: HashMap::new(),
            pty_handles: HashMap::new(),
            default_mode: PromptMode::Interactive,
            max_saved_prompts: 100,
            prompts_dir: None,
            worktree_cleanup: WorktreeCleanup::Manual,
        }
    }

    // ── Persistence ──

    pub fn persist_prompt(&self, prompt: &Prompt) {
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(
                dir,
                &prompt.uuid,
                &persistence::PromptFile::from_prompt(prompt),
            );
        }
    }

    pub fn persist_prompt_by_id(&self, prompt_id: usize) {
        if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
            self.persist_prompt(prompt);
        }
    }

    // ── Counts ──

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

    // ── Prompt lifecycle ──

    /// Add a new prompt. Does NOT rebuild filter — caller must do that.
    pub fn add_prompt(
        &mut self,
        text: String,
        cwd: Option<String>,
        worktree: bool,
        tags: Vec<String>,
    ) {
        let mut prompt = Prompt::new(self.next_id, text, cwd, self.default_mode);
        prompt.worktree = worktree;
        prompt.tags = tags;
        let max_rank = self
            .prompts
            .iter()
            .map(|p| p.queue_rank)
            .fold(0.0_f64, f64::max);
        prompt.queue_rank = max_rank + 1.0;
        self.next_id += 1;
        self.persist_prompt(&prompt);
        self.prompts.push(prompt);
    }

    pub fn next_pending_prompt_index(&self) -> Option<usize> {
        self.prompts
            .iter()
            .position(|p| p.status == PromptStatus::Pending)
    }

    pub fn mark_running(&mut self, index: usize) {
        if let Some(prompt) = self.prompts.get_mut(index) {
            prompt.status = PromptStatus::Running;
            prompt.mark_started();
        }
        if let Some(prompt) = self.prompts.get(index) {
            self.persist_prompt(prompt);
        }
    }

    // ── Message handling ──

    pub fn apply_message(&mut self, msg: WorkerMessage) -> MsgEffect {
        match msg {
            WorkerMessage::OutputChunk { prompt_id, text } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    if prompt.status == PromptStatus::Idle {
                        prompt.status = PromptStatus::Running;
                    }
                    match &mut prompt.output {
                        Some(existing) => existing.push_str(&text),
                        None => prompt.output = Some(text),
                    }
                }
                MsgEffect::None
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
                MsgEffect::None
            }
            WorkerMessage::PtyUpdate { .. } => MsgEffect::None,
            WorkerMessage::SessionId {
                prompt_id,
                session_id,
            } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.session_id = Some(session_id);
                }
                self.persist_prompt_by_id(prompt_id);
                MsgEffect::None
            }
            WorkerMessage::Finished {
                prompt_id,
                exit_code,
            } => {
                // For PTY workers: extract text from terminal grid
                let pty_text = if let Some(handle) = self.pty_handles.get(&prompt_id) {
                    let text = pty_worker::extract_text_from_term(&handle.state);
                    if !text.is_empty() {
                        Some(text)
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    if let Some(text) = pty_text {
                        prompt.output = Some(text);
                    } else if let Some(output) = &mut prompt.output {
                        output.push('\n');
                    }

                    prompt.mark_finished();
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

                MsgEffect::PromptFinished { prompt_id }
            }
            WorkerMessage::SpawnError { prompt_id, error } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.status = PromptStatus::Failed;
                    prompt.mark_finished();
                    prompt.error = Some(error);
                }
                self.persist_prompt_by_id(prompt_id);
                self.maybe_cleanup_worktree(prompt_id);
                self.pty_handles.remove(&prompt_id);
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);
                MsgEffect::None
            }
        }
    }

    // ── Retry / Resume ──

    /// Create a new pending prompt by retrying (cloning) an existing one.
    /// No selection logic — caller provides the data.
    pub fn retry_prompt(
        &mut self,
        text: String,
        cwd: Option<String>,
        mode: PromptMode,
        worktree: bool,
        tags: Vec<String>,
    ) {
        let mut new_prompt = Prompt::new(self.next_id, text, cwd, mode);
        new_prompt.worktree = worktree;
        new_prompt.tags = tags;
        let max_rank = self
            .prompts
            .iter()
            .map(|p| p.queue_rank)
            .fold(0.0_f64, f64::max);
        new_prompt.queue_rank = max_rank + 1.0;
        self.next_id += 1;
        self.persist_prompt(&new_prompt);
        self.prompts.push(new_prompt);
    }

    /// Resume a prompt at the given index. No selection logic — caller provides the index.
    pub fn resume_prompt(&mut self, idx: usize) {
        let Some(prompt) = self.prompts.get_mut(idx) else {
            return;
        };
        if prompt.status != PromptStatus::Completed && prompt.status != PromptStatus::Failed {
            return;
        }
        prompt.status = PromptStatus::Pending;
        prompt.resume = true;
        prompt.output = None;
        prompt.error = None;
        prompt.started_at_ms = None;
        prompt.finished_at_ms = None;
        prompt.seen = false;
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(
                dir,
                &self.prompts[idx].uuid,
                &persistence::PromptFile::from_prompt(&self.prompts[idx]),
            );
        }
    }

    // ── Reorder ──

    pub fn move_prompt_up(&mut self, idx: usize) -> MoveResult {
        if idx == 0 {
            return MoveResult::NotMoved;
        }
        if self.prompts[idx].status != PromptStatus::Pending {
            return MoveResult::NotMoved;
        }
        let rank_a = self.prompts[idx].queue_rank;
        let rank_b = self.prompts[idx - 1].queue_rank;
        self.prompts[idx].queue_rank = rank_b;
        self.prompts[idx - 1].queue_rank = rank_a;
        self.prompts.swap(idx, idx - 1);
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(
                dir,
                &self.prompts[idx].uuid,
                &persistence::PromptFile::from_prompt(&self.prompts[idx]),
            );
            persistence::save_prompt(
                dir,
                &self.prompts[idx - 1].uuid,
                &persistence::PromptFile::from_prompt(&self.prompts[idx - 1]),
            );
        }
        let prompt_id = self.prompts[idx - 1].id;
        MoveResult::Moved { prompt_id }
    }

    pub fn move_prompt_down(&mut self, idx: usize) -> MoveResult {
        if idx >= self.prompts.len() - 1 {
            return MoveResult::NotMoved;
        }
        if self.prompts[idx].status != PromptStatus::Pending {
            return MoveResult::NotMoved;
        }
        let rank_a = self.prompts[idx].queue_rank;
        let rank_b = self.prompts[idx + 1].queue_rank;
        self.prompts[idx].queue_rank = rank_b;
        self.prompts[idx + 1].queue_rank = rank_a;
        self.prompts.swap(idx, idx + 1);
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(
                dir,
                &self.prompts[idx].uuid,
                &persistence::PromptFile::from_prompt(&self.prompts[idx]),
            );
            persistence::save_prompt(
                dir,
                &self.prompts[idx + 1].uuid,
                &persistence::PromptFile::from_prompt(&self.prompts[idx + 1]),
            );
        }
        let prompt_id = self.prompts[idx + 1].id;
        MoveResult::Moved { prompt_id }
    }

    // ── Kill ──

    pub fn kill_worker(&mut self, id: usize) {
        if let Some(sender) = self.worker_inputs.get(&id) {
            let _ = sender.send(WorkerInput::Kill);
        }
        if let Some(mut handle) = self.pty_handles.remove(&id) {
            let _ = handle.child.kill();
        }
    }

    // ── Batch operations ──

    /// Batch retry: returns the count of retried prompts.
    #[allow(clippy::type_complexity)]
    pub fn batch_retry_prompts(&mut self, ids: &std::collections::HashSet<usize>) -> usize {
        let to_retry: Vec<(String, Option<String>, PromptMode, bool, Vec<String>)> = self
            .prompts
            .iter()
            .filter(|p| {
                ids.contains(&p.id)
                    && (p.status == PromptStatus::Completed || p.status == PromptStatus::Failed)
            })
            .map(|p| {
                (
                    p.text.clone(),
                    p.cwd.clone(),
                    p.mode,
                    p.worktree,
                    p.tags.clone(),
                )
            })
            .collect();
        let count = to_retry.len();
        for (text, cwd, mode, wt, tags) in to_retry {
            self.retry_prompt(text, cwd, mode, wt, tags);
        }
        count
    }

    /// Batch kill: returns the count of killed workers.
    pub fn batch_kill_prompts(&mut self, ids: &std::collections::HashSet<usize>) -> usize {
        let kill_ids: Vec<usize> = self
            .prompts
            .iter()
            .filter(|p| {
                ids.contains(&p.id)
                    && (p.status == PromptStatus::Running || p.status == PromptStatus::Idle)
            })
            .map(|p| p.id)
            .collect();
        let count = kill_ids.len();
        for id in kill_ids {
            self.kill_worker(id);
        }
        count
    }

    /// Batch delete: returns the count of deleted prompts.
    pub fn batch_delete_prompts(&mut self, ids: &std::collections::HashSet<usize>) -> usize {
        let id_list: Vec<usize> = ids.iter().copied().collect();
        let mut count = 0;
        for id in id_list {
            // Kill running/idle workers first
            if let Some(prompt) = self.prompts.iter().find(|p| p.id == id) {
                if prompt.status == PromptStatus::Running || prompt.status == PromptStatus::Idle {
                    self.kill_worker(id);
                    self.worker_inputs.remove(&id);
                    self.active_workers = self.active_workers.saturating_sub(1);
                }
            }
            // Delete persistence file
            if let Some(ref dir) = self.prompts_dir {
                if let Some(prompt) = self.prompts.iter().find(|p| p.id == id) {
                    persistence::delete_prompt_file(dir, &prompt.uuid);
                }
            }
            // Remove from prompts list
            if let Some(pos) = self.prompts.iter().position(|p| p.id == id) {
                self.prompts.remove(pos);
                count += 1;
            }
        }
        count
    }

    /// Batch toggle mode: returns the count of toggled prompts.
    pub fn batch_toggle_mode_prompts(&mut self, ids: &std::collections::HashSet<usize>) -> usize {
        let toggle_ids: Vec<usize> = self
            .prompts
            .iter()
            .filter(|p| ids.contains(&p.id) && p.status == PromptStatus::Pending)
            .map(|p| p.id)
            .collect();
        let count = toggle_ids.len();
        for id in &toggle_ids {
            if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == *id) {
                prompt.mode = prompt.mode.toggle();
            }
        }
        for id in &toggle_ids {
            self.persist_prompt_by_id(*id);
        }
        count
    }

    // ── Interact helpers ──

    /// Send text input to a one-shot worker, echoing it to the prompt output.
    pub fn send_interact_input(&mut self, idx: usize, text: &str) {
        let Some(prompt) = self.prompts.get_mut(idx) else {
            return;
        };
        let id = prompt.id;
        let Some(sender) = self.worker_inputs.get(&id) else {
            return;
        };
        let echo = format!("\n\n> {text}\n\n");
        match &mut prompt.output {
            Some(existing) => existing.push_str(&echo),
            None => prompt.output = Some(echo),
        }
        let mut send_text = text.to_string();
        send_text.push('\n');
        let _ = sender.send(WorkerInput::SendInput(send_text));
    }

    /// Send a quick prompt to a worker (PTY or one-shot).
    pub fn try_quick_prompt_send(&mut self, id: usize, message: &str, is_pty: bool) {
        let Some(sender) = self.worker_inputs.get(&id) else {
            return;
        };
        if is_pty {
            let mut bytes = message.as_bytes().to_vec();
            bytes.push(b'\r');
            let _ = sender.send(WorkerInput::SendBytes(bytes));
        } else {
            // Echo and send
            if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == id) {
                let echo = format!("\n\n> {message}\n\n");
                match &mut prompt.output {
                    Some(existing) => existing.push_str(&echo),
                    None => prompt.output = Some(echo),
                }
            }
            let mut send_text = message.to_string();
            send_text.push('\n');
            let _ = sender.send(WorkerInput::SendInput(send_text));
        }
    }

    /// Send raw bytes to a PTY worker.
    pub fn send_pty_bytes(&self, id: usize, bytes: Vec<u8>) {
        if let Some(sender) = self.worker_inputs.get(&id) {
            let _ = sender.send(WorkerInput::SendBytes(bytes));
        }
    }

    // ── PTY resize ──

    pub fn resize_pty_workers(&mut self, cols: u16, rows: u16) {
        for handle in self.pty_handles.values() {
            pty_worker::resize_pty(handle, cols, rows);
        }
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
                persistence::save_prompt(
                    dir,
                    &prompt.uuid,
                    &persistence::PromptFile::from_prompt(prompt),
                );
            }
        }
        // Spawn a background thread for cleanup to avoid blocking
        let wt_path = PathBuf::from(&wt_path);
        std::thread::spawn(move || {
            if let Some(parent) = wt_path.parent() {
                if let Ok(entries) = std::fs::read_dir(parent) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir()
                            && path != wt_path
                            && clhorde_core::worktree::is_git_repo(&path)
                        {
                            if let Some(root) = clhorde_core::worktree::repo_root(&path) {
                                let _ = clhorde_core::worktree::remove_worktree(&root, &wt_path);
                                return;
                            }
                        }
                    }
                }
            }
        });
    }

    // ── Shutdown ──

    pub fn shutdown(&mut self) {
        for (_id, sender) in self.worker_inputs.drain() {
            let _ = sender.send(WorkerInput::Kill);
        }
        self.pty_handles.clear();
    }
}
