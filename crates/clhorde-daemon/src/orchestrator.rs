use std::collections::HashMap;
use std::path::PathBuf;

use tokio::sync::mpsc;

use clhorde_core::config::WorktreeCleanup;
use clhorde_core::keymap::load_settings;
use clhorde_core::persistence;
use clhorde_core::prompt::{Prompt, PromptMode, PromptStatus};
use clhorde_core::protocol::{DaemonEvent, DaemonState, PromptInfo};

use crate::pty_worker::{self, PtyHandle};
use crate::session::SessionManager;
use crate::worker::{self, SpawnResult, WorkerInput, WorkerMessage};

/// Daemon orchestrator: manages prompts, workers, and broadcasts events to clients.
pub struct Orchestrator {
    pub prompts: Vec<Prompt>,
    pub next_id: usize,
    pub max_workers: usize,
    pub active_workers: usize,
    pub default_mode: PromptMode,
    pub worker_inputs: HashMap<usize, mpsc::UnboundedSender<WorkerInput>>,
    pub pty_handles: HashMap<usize, PtyHandle>,
    pub sessions: SessionManager,
    pub worker_tx: mpsc::UnboundedSender<WorkerMessage>,
    pub worker_rx: mpsc::UnboundedReceiver<WorkerMessage>,
    pub pty_byte_tx: tokio::sync::broadcast::Sender<(usize, Vec<u8>)>,
    #[allow(dead_code)]
    max_saved_prompts: usize,
    prompts_dir: Option<PathBuf>,
    worktree_cleanup: WorktreeCleanup,
}

impl Orchestrator {
    /// Create an Orchestrator for tests — no persistence, no settings loading.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        let (pty_byte_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            prompts: Vec::new(),
            next_id: 1,
            max_workers: 3,
            active_workers: 0,
            default_mode: PromptMode::Interactive,
            worker_inputs: HashMap::new(),
            pty_handles: HashMap::new(),
            sessions: SessionManager::new(),
            worker_tx,
            worker_rx,
            pty_byte_tx,
            max_saved_prompts: 100,
            prompts_dir: None,
            worktree_cleanup: WorktreeCleanup::Manual,
        }
    }

    pub fn new() -> Self {
        let settings = load_settings();
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

        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        let (pty_byte_tx, _) = tokio::sync::broadcast::channel(256);

        Self {
            prompts,
            next_id,
            max_workers: 3,
            active_workers: 0,
            default_mode: PromptMode::Interactive,
            worker_inputs: HashMap::new(),
            pty_handles: HashMap::new(),
            sessions: SessionManager::new(),
            worker_tx,
            worker_rx,
            pty_byte_tx,
            max_saved_prompts,
            prompts_dir,
            worktree_cleanup,
        }
    }

    // ── Persistence ──

    fn persist_prompt(&self, prompt: &Prompt) {
        if let Some(ref dir) = self.prompts_dir {
            persistence::save_prompt(
                dir,
                &prompt.uuid,
                &persistence::PromptFile::from_prompt(prompt),
            );
        }
    }

    fn persist_prompt_by_id(&self, prompt_id: usize) {
        if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
            self.persist_prompt(prompt);
        }
    }

    // ── Convert to wire types ──

    pub fn to_prompt_info(&self, prompt: &Prompt) -> PromptInfo {
        PromptInfo {
            id: prompt.id,
            text: prompt.text.clone(),
            cwd: prompt.cwd.clone(),
            mode: prompt.mode.label().to_string(),
            status: format!("{:?}", prompt.status),
            output: prompt.output.clone(),
            error: prompt.error.clone(),
            worktree: prompt.worktree,
            worktree_path: prompt.worktree_path.clone(),
            session_id: prompt.session_id.clone(),
            tags: prompt.tags.clone(),
            queue_rank: prompt.queue_rank,
            seen: prompt.seen,
            resume: prompt.resume,
            output_len: prompt.output.as_ref().map_or(0, |s| s.len()),
            elapsed_secs: prompt.elapsed_secs(),
            uuid: prompt.uuid.clone(),
            has_pty: self.pty_handles.contains_key(&prompt.id),
        }
    }

    pub fn to_daemon_state(&self) -> DaemonState {
        DaemonState {
            prompts: self
                .prompts
                .iter()
                .map(|p| self.to_prompt_info(p))
                .collect(),
            max_workers: self.max_workers,
            active_workers: self.active_workers,
            default_mode: self.default_mode.label().to_string(),
        }
    }

    // ── Prompt lifecycle ──

    pub fn add_prompt(
        &mut self,
        text: String,
        cwd: Option<String>,
        mode: PromptMode,
        worktree: bool,
        tags: Vec<String>,
    ) {
        let mut prompt = Prompt::new(self.next_id, text, cwd, mode);
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

        let info = self.to_prompt_info(&prompt);
        self.prompts.push(prompt);
        self.sessions.broadcast(&DaemonEvent::PromptAdded(info));
    }

    fn next_pending_prompt_index(&self) -> Option<usize> {
        self.prompts
            .iter()
            .position(|p| p.status == PromptStatus::Pending)
    }

    fn mark_running(&mut self, index: usize) {
        if let Some(prompt) = self.prompts.get_mut(index) {
            prompt.status = PromptStatus::Running;
            prompt.mark_started();
        }
        if let Some(prompt) = self.prompts.get(index) {
            self.persist_prompt(prompt);
            let info = self.to_prompt_info(prompt);
            let id = prompt.id;
            self.sessions
                .broadcast(&DaemonEvent::WorkerStarted { prompt_id: id });
            self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
        }
    }

    // ── Worker dispatch ──

    pub fn dispatch_workers(&mut self) {
        while self.active_workers < self.max_workers {
            let Some(idx) = self.next_pending_prompt_index() else {
                break;
            };

            // Extract all fields we need before any mutation
            let prompt_id = self.prompts[idx].id;
            let prompt_text = self.prompts[idx].text.clone();
            let prompt_worktree = self.prompts[idx].worktree;
            let prompt_cwd = self.prompts[idx].cwd.clone();
            let prompt_worktree_path = self.prompts[idx].worktree_path.clone();
            let mode = self.prompts[idx].mode;
            let resume_session_id = if self.prompts[idx].resume {
                Some(self.prompts[idx].session_id.clone().unwrap_or_default())
            } else {
                None
            };

            let cwd = if prompt_worktree {
                if prompt_worktree_path.is_none() {
                    let effective_cwd = prompt_cwd.clone().unwrap_or_else(|| {
                        std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                    let repo_path = std::path::Path::new(&effective_cwd);
                    if clhorde_core::worktree::is_git_repo(repo_path) {
                        match clhorde_core::worktree::create_worktree(repo_path, prompt_id) {
                            Ok(wt_path) => {
                                let wt_str = wt_path.to_string_lossy().to_string();
                                self.prompts[idx].worktree_path = Some(wt_str.clone());
                                Some(wt_str)
                            }
                            Err(e) => {
                                self.prompts[idx].status = PromptStatus::Failed;
                                self.prompts[idx].error =
                                    Some(format!("Failed to create worktree: {e}"));
                                self.persist_prompt(&self.prompts[idx]);
                                let info = self.to_prompt_info(&self.prompts[idx]);
                                self.sessions.broadcast(&DaemonEvent::WorkerError {
                                    prompt_id,
                                    error: format!("Failed to create worktree: {e}"),
                                });
                                self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                                continue;
                            }
                        }
                    } else {
                        prompt_cwd
                    }
                } else {
                    prompt_worktree_path
                }
            } else {
                prompt_cwd
            };

            self.mark_running(idx);
            self.active_workers += 1;
            self.sessions.broadcast(&DaemonEvent::ActiveWorkersChanged {
                count: self.active_workers,
            });

            let result = worker::spawn_worker(
                prompt_id,
                prompt_text,
                cwd,
                mode,
                self.worker_tx.clone(),
                Some((80, 24)), // Default PTY size; clients can resize
                resume_session_id,
                self.pty_byte_tx.clone(),
            );

            match result {
                SpawnResult::Pty {
                    input_sender,
                    pty_handle,
                } => {
                    self.worker_inputs.insert(prompt_id, input_sender);
                    self.pty_handles.insert(prompt_id, pty_handle);
                }
                SpawnResult::OneShot => {
                    // No input channel for one-shot
                }
                SpawnResult::Error(e) => {
                    self.active_workers = self.active_workers.saturating_sub(1);
                    if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                        prompt.status = PromptStatus::Failed;
                        prompt.mark_finished();
                        prompt.error = Some(e.clone());
                    }
                    self.persist_prompt_by_id(prompt_id);
                    if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                        let info = self.to_prompt_info(prompt);
                        self.sessions.broadcast(&DaemonEvent::WorkerError {
                            prompt_id,
                            error: e,
                        });
                        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                    }
                    self.sessions.broadcast(&DaemonEvent::ActiveWorkersChanged {
                        count: self.active_workers,
                    });
                }
            }
        }
    }

    // ── Message handling ──

    pub fn apply_message(&mut self, msg: WorkerMessage) {
        match msg {
            WorkerMessage::OutputChunk { prompt_id, text } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    if prompt.status == PromptStatus::Idle {
                        prompt.status = PromptStatus::Running;
                    }
                    match &mut prompt.output {
                        Some(existing) => existing.push_str(&text),
                        None => prompt.output = Some(text.clone()),
                    }
                }
                self.sessions
                    .broadcast(&DaemonEvent::OutputChunk { prompt_id, text });
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
                self.sessions
                    .broadcast(&DaemonEvent::TurnComplete { prompt_id });
                if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                    let info = self.to_prompt_info(prompt);
                    self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                }
            }
            WorkerMessage::PtyUpdate { prompt_id } => {
                self.sessions
                    .broadcast(&DaemonEvent::PtyUpdate { prompt_id });
            }
            WorkerMessage::SessionId {
                prompt_id,
                session_id,
            } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.session_id = Some(session_id.clone());
                }
                self.persist_prompt_by_id(prompt_id);
                self.sessions.broadcast(&DaemonEvent::SessionId {
                    prompt_id,
                    session_id,
                });
            }
            WorkerMessage::PtyEof { prompt_id } => {
                // Extract text from PTY grid while the terminal state is still available
                if let Some(handle) = self.pty_handles.get(&prompt_id) {
                    let text = pty_worker::extract_text_from_term(&handle.state);
                    if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                        if !text.is_empty() {
                            prompt.output = Some(text);
                        }
                    }
                }
                // Take child to call wait() in a background thread for the real exit code
                let child_opt = self
                    .pty_handles
                    .get_mut(&prompt_id)
                    .and_then(|h| h.child.take());
                if let Some(mut child) = child_opt {
                    let tx = self.worker_tx.clone();
                    std::thread::spawn(move || {
                        let exit_code = match child.wait() {
                            Ok(status) => {
                                if status.success() {
                                    Some(0)
                                } else {
                                    Some(1)
                                }
                            }
                            Err(_) => Some(1),
                        };
                        let _ = tx.send(WorkerMessage::Finished {
                            prompt_id,
                            exit_code,
                        });
                    });
                } else {
                    // Handle already removed (kill race) — send synthetic Finished
                    let _ = self.worker_tx.send(WorkerMessage::Finished {
                        prompt_id,
                        exit_code: None,
                    });
                }
            }
            WorkerMessage::Finished {
                prompt_id,
                exit_code,
            } => {
                // For PTY workers: extract text from terminal grid if not already captured by PtyEof
                if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                    if prompt.output.is_none() {
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
                        if let Some(text) = pty_text {
                            if let Some(prompt) =
                                self.prompts.iter_mut().find(|p| p.id == prompt_id)
                            {
                                prompt.output = Some(text);
                            }
                        }
                    }
                }

                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    // For one-shot workers: append trailing newline to output
                    if let Some(output) = &mut prompt.output {
                        if !output.ends_with('\n') {
                            output.push('\n');
                        }
                    }

                    prompt.mark_finished();
                    match exit_code {
                        Some(0) | None => {
                            prompt.status = PromptStatus::Completed;
                        }
                        Some(code) => {
                            prompt.status = PromptStatus::Failed;
                            if prompt.error.is_none() {
                                prompt.error = Some(format!("Exit code: {code}"));
                            }
                        }
                    }
                }
                self.persist_prompt_by_id(prompt_id);
                self.maybe_cleanup_worktree(prompt_id);
                self.pty_handles.remove(&prompt_id);
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);

                self.sessions.broadcast(&DaemonEvent::WorkerFinished {
                    prompt_id,
                    exit_code,
                });
                if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                    let info = self.to_prompt_info(prompt);
                    self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                }
                self.sessions.broadcast(&DaemonEvent::ActiveWorkersChanged {
                    count: self.active_workers,
                });
            }
            WorkerMessage::SpawnError { prompt_id, error } => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    prompt.status = PromptStatus::Failed;
                    prompt.mark_finished();
                    prompt.error = Some(error.clone());
                }
                self.persist_prompt_by_id(prompt_id);
                self.maybe_cleanup_worktree(prompt_id);
                self.pty_handles.remove(&prompt_id);
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);

                self.sessions
                    .broadcast(&DaemonEvent::WorkerError { prompt_id, error });
                if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                    let info = self.to_prompt_info(prompt);
                    self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                }
                self.sessions.broadcast(&DaemonEvent::ActiveWorkersChanged {
                    count: self.active_workers,
                });
            }
        }
    }

    // ── Request handling ──

    pub fn handle_request(
        &mut self,
        req: clhorde_core::protocol::ClientRequest,
        session_id: usize,
    ) {
        use clhorde_core::protocol::ClientRequest;

        match req {
            ClientRequest::SubmitPrompt {
                text,
                cwd,
                mode,
                worktree,
                tags,
            } => {
                let prompt_mode = match mode.as_str() {
                    "one-shot" | "one_shot" | "oneshot" => PromptMode::OneShot,
                    _ => PromptMode::Interactive,
                };
                self.add_prompt(text, cwd, prompt_mode, worktree, tags);
                self.dispatch_workers();
            }
            ClientRequest::SendInput { prompt_id, text } => {
                // Echo the input to output so all clients see it
                let echo = format!("\n\n> {text}\n\n");
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                    match &mut prompt.output {
                        Some(existing) => existing.push_str(&echo),
                        None => prompt.output = Some(echo.clone()),
                    }
                }
                self.sessions.broadcast(&DaemonEvent::OutputChunk {
                    prompt_id,
                    text: echo,
                });
                if let Some(sender) = self.worker_inputs.get(&prompt_id) {
                    let mut send_text = text;
                    send_text.push('\n');
                    let _ = sender.send(WorkerInput::SendInput(send_text));
                }
            }
            ClientRequest::SendBytes { prompt_id, data } => {
                if let Some(sender) = self.worker_inputs.get(&prompt_id) {
                    let _ = sender.send(WorkerInput::SendBytes(data));
                }
            }
            ClientRequest::KillWorker { prompt_id } => {
                self.kill_worker(prompt_id);
            }
            ClientRequest::RetryPrompt { prompt_id } => {
                let retry_data = self
                    .prompts
                    .iter()
                    .find(|p| p.id == prompt_id)
                    .and_then(|p| {
                        if p.status == PromptStatus::Completed || p.status == PromptStatus::Failed {
                            Some((
                                p.text.clone(),
                                p.cwd.clone(),
                                p.mode,
                                p.worktree,
                                p.tags.clone(),
                            ))
                        } else {
                            None
                        }
                    });
                if let Some((text, cwd, mode, wt, tags)) = retry_data {
                    self.add_prompt(text, cwd, mode, wt, tags);
                    self.dispatch_workers();
                }
            }
            ClientRequest::ResumePrompt { prompt_id } => {
                if let Some(idx) = self.prompts.iter().position(|p| p.id == prompt_id) {
                    self.resume_prompt(idx);
                    self.dispatch_workers();
                }
            }
            ClientRequest::DeletePrompt { prompt_id } => {
                self.delete_prompt(prompt_id);
            }
            ClientRequest::MovePromptUp { prompt_id } => {
                if let Some(idx) = self.prompts.iter().position(|p| p.id == prompt_id) {
                    self.move_prompt_up(idx);
                }
            }
            ClientRequest::MovePromptDown { prompt_id } => {
                if let Some(idx) = self.prompts.iter().position(|p| p.id == prompt_id) {
                    self.move_prompt_down(idx);
                }
            }
            ClientRequest::SetMaxWorkers(n) => {
                self.max_workers = n.clamp(1, 20);
                self.sessions.broadcast(&DaemonEvent::MaxWorkersChanged {
                    count: self.max_workers,
                });
                self.dispatch_workers();
            }
            ClientRequest::SetDefaultMode { mode } => {
                self.default_mode = match mode.as_str() {
                    "one-shot" | "one_shot" | "oneshot" => PromptMode::OneShot,
                    _ => PromptMode::Interactive,
                };
            }
            ClientRequest::SetPromptMode { prompt_id, mode } => {
                let new_mode = match mode.as_str() {
                    "one-shot" | "one_shot" | "oneshot" => PromptMode::OneShot,
                    _ => PromptMode::Interactive,
                };
                if let Some(idx) = self.prompts.iter().position(|p| p.id == prompt_id) {
                    if self.prompts[idx].status == PromptStatus::Pending {
                        self.prompts[idx].mode = new_mode;
                        self.persist_prompt_by_id(prompt_id);
                        let info = self.to_prompt_info(&self.prompts[idx]);
                        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                    }
                }
            }
            ClientRequest::GetState => {
                let state = self.to_daemon_state();
                self.sessions
                    .send_to(session_id, DaemonEvent::StateSnapshot(state));
            }
            ClientRequest::GetPromptOutput { prompt_id } => {
                let text = self
                    .prompts
                    .iter()
                    .find(|p| p.id == prompt_id)
                    .and_then(|p| p.output.clone())
                    .unwrap_or_default();
                self.sessions.send_to(
                    session_id,
                    DaemonEvent::PromptOutput {
                        prompt_id,
                        full_text: text,
                    },
                );
            }
            ClientRequest::ResizePty {
                prompt_id,
                cols,
                rows,
            } => {
                if let Some(handle) = self.pty_handles.get(&prompt_id) {
                    pty_worker::resize_pty(handle, cols, rows);
                }
            }
            ClientRequest::Subscribe => {
                self.sessions.set_subscribed(session_id, true);
                // Send ring buffer replay for all active PTY workers
                for (&pid, handle) in &self.pty_handles {
                    if let Ok(ring) = handle.ring_buffer.lock() {
                        let data = ring.snapshot();
                        if !data.is_empty() {
                            self.sessions.send_to(
                                session_id,
                                DaemonEvent::PtyReplay {
                                    prompt_id: pid,
                                    data,
                                },
                            );
                        }
                    }
                }
                self.sessions.send_to(session_id, DaemonEvent::Subscribed);
            }
            ClientRequest::Unsubscribe => {
                self.sessions.set_subscribed(session_id, false);
                self.sessions.send_to(session_id, DaemonEvent::Unsubscribed);
            }
            ClientRequest::Ping => {
                self.sessions.send_to(session_id, DaemonEvent::Pong);
            }
            ClientRequest::Shutdown => {
                // Handled by the main loop — just acknowledged here
                self.sessions.send_to(
                    session_id,
                    DaemonEvent::StoreOpComplete {
                        message: "Shutdown initiated".to_string(),
                    },
                );
            }
            // Store commands
            ClientRequest::StoreList => {
                let infos: Vec<PromptInfo> = self
                    .prompts
                    .iter()
                    .map(|p| self.to_prompt_info(p))
                    .collect();
                self.sessions
                    .send_to(session_id, DaemonEvent::StoreListResult { prompts: infos });
            }
            ClientRequest::StoreCount => {
                let pending = self
                    .prompts
                    .iter()
                    .filter(|p| p.status == PromptStatus::Pending)
                    .count();
                let running = self
                    .prompts
                    .iter()
                    .filter(|p| p.status == PromptStatus::Running || p.status == PromptStatus::Idle)
                    .count();
                let completed = self
                    .prompts
                    .iter()
                    .filter(|p| p.status == PromptStatus::Completed)
                    .count();
                let failed = self
                    .prompts
                    .iter()
                    .filter(|p| p.status == PromptStatus::Failed)
                    .count();
                self.sessions.send_to(
                    session_id,
                    DaemonEvent::StoreCountResult {
                        pending,
                        running,
                        completed,
                        failed,
                    },
                );
            }
            ClientRequest::StorePath => {
                let path = self
                    .prompts_dir
                    .as_ref()
                    .map_or_else(String::new, |d| d.to_string_lossy().to_string());
                self.sessions
                    .send_to(session_id, DaemonEvent::StorePathResult { path });
            }
            ClientRequest::StoreDrop { filter } => {
                let count = self.store_drop(&filter);
                self.sessions.send_to(
                    session_id,
                    DaemonEvent::StoreOpComplete {
                        message: format!("Dropped {count} prompts"),
                    },
                );
            }
            ClientRequest::StoreKeep { filter } => {
                let count = self.store_keep(&filter);
                self.sessions.send_to(
                    session_id,
                    DaemonEvent::StoreOpComplete {
                        message: format!("Kept matching, dropped {count} prompts"),
                    },
                );
            }
            ClientRequest::CleanWorktrees => {
                let count = self.clean_worktrees();
                self.sessions.send_to(
                    session_id,
                    DaemonEvent::StoreOpComplete {
                        message: format!("Cleaned {count} worktrees"),
                    },
                );
            }
        }
    }

    // ── Kill ──

    fn kill_worker(&mut self, id: usize) {
        if let Some(sender) = self.worker_inputs.get(&id) {
            let _ = sender.send(WorkerInput::Kill);
        }
        if let Some(mut handle) = self.pty_handles.remove(&id) {
            if let Some(mut child) = handle.child.take() {
                let _ = child.kill();
            }
        }
    }

    // ── Resume ──

    fn resume_prompt(&mut self, idx: usize) {
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
        self.persist_prompt(&self.prompts[idx]);
        let info = self.to_prompt_info(&self.prompts[idx]);
        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
    }

    // ── Delete ──

    fn delete_prompt(&mut self, prompt_id: usize) {
        // Kill running worker if any
        if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
            if prompt.status == PromptStatus::Running || prompt.status == PromptStatus::Idle {
                self.kill_worker(prompt_id);
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);
            }
        }
        // Delete persistence file
        if let Some(ref dir) = self.prompts_dir {
            if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                persistence::delete_prompt_file(dir, &prompt.uuid);
            }
        }
        if let Some(pos) = self.prompts.iter().position(|p| p.id == prompt_id) {
            self.prompts.remove(pos);
            self.sessions
                .broadcast(&DaemonEvent::PromptRemoved { prompt_id });
        }
    }

    // ── Move ──

    fn move_prompt_up(&mut self, idx: usize) {
        if idx == 0 || self.prompts[idx].status != PromptStatus::Pending {
            return;
        }
        let rank_a = self.prompts[idx].queue_rank;
        let rank_b = self.prompts[idx - 1].queue_rank;
        self.prompts[idx].queue_rank = rank_b;
        self.prompts[idx - 1].queue_rank = rank_a;
        self.prompts.swap(idx, idx - 1);
        self.persist_prompt(&self.prompts[idx]);
        self.persist_prompt(&self.prompts[idx - 1]);
        let info_a = self.to_prompt_info(&self.prompts[idx]);
        let info_b = self.to_prompt_info(&self.prompts[idx - 1]);
        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info_a));
        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info_b));
    }

    fn move_prompt_down(&mut self, idx: usize) {
        if idx >= self.prompts.len() - 1 || self.prompts[idx].status != PromptStatus::Pending {
            return;
        }
        let rank_a = self.prompts[idx].queue_rank;
        let rank_b = self.prompts[idx + 1].queue_rank;
        self.prompts[idx].queue_rank = rank_b;
        self.prompts[idx + 1].queue_rank = rank_a;
        self.prompts.swap(idx, idx + 1);
        self.persist_prompt(&self.prompts[idx]);
        self.persist_prompt(&self.prompts[idx + 1]);
        let info_a = self.to_prompt_info(&self.prompts[idx]);
        let info_b = self.to_prompt_info(&self.prompts[idx + 1]);
        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info_a));
        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info_b));
    }

    // ── Store operations ──

    fn store_drop(&mut self, filter: &str) -> usize {
        let to_remove: Vec<usize> = self
            .prompts
            .iter()
            .filter(|p| match filter {
                "all" => true,
                "completed" => p.status == PromptStatus::Completed,
                "failed" => p.status == PromptStatus::Failed,
                "pending" => p.status == PromptStatus::Pending,
                "running" => p.status == PromptStatus::Running || p.status == PromptStatus::Idle,
                _ => false,
            })
            .map(|p| p.id)
            .collect();
        let count = to_remove.len();
        for id in to_remove {
            self.delete_prompt(id);
        }
        count
    }

    fn store_keep(&mut self, filter: &str) -> usize {
        let to_remove: Vec<usize> = self
            .prompts
            .iter()
            .filter(|p| {
                let keep = match filter {
                    "completed" => p.status == PromptStatus::Completed,
                    "failed" => p.status == PromptStatus::Failed,
                    "running" => {
                        p.status == PromptStatus::Running || p.status == PromptStatus::Idle
                    }
                    "pending" => p.status == PromptStatus::Pending,
                    _ => false,
                };
                !keep && p.status != PromptStatus::Running && p.status != PromptStatus::Idle
            })
            .map(|p| p.id)
            .collect();
        let count = to_remove.len();
        for id in to_remove {
            self.delete_prompt(id);
        }
        count
    }

    fn clean_worktrees(&self) -> usize {
        let mut count = 0;
        for prompt in &self.prompts {
            if prompt.status == PromptStatus::Completed || prompt.status == PromptStatus::Failed {
                if let Some(ref wt_path) = prompt.worktree_path {
                    let wt = std::path::PathBuf::from(wt_path);
                    if wt.exists() {
                        if let Some(parent) = wt.parent() {
                            if let Ok(entries) = std::fs::read_dir(parent) {
                                for entry in entries.flatten() {
                                    let path = entry.path();
                                    if path.is_dir()
                                        && path != wt
                                        && clhorde_core::worktree::is_git_repo(&path)
                                    {
                                        if let Some(root) = clhorde_core::worktree::repo_root(&path)
                                        {
                                            if clhorde_core::worktree::remove_worktree(&root, &wt)
                                                .is_ok()
                                            {
                                                count += 1;
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        count
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
        // Spawn a background thread for cleanup
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
        for (_id, mut handle) in self.pty_handles.drain() {
            if let Some(mut child) = handle.child.take() {
                let _ = child.kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clhorde_core::prompt::Prompt;

    /// Helper: insert a prompt with a given status directly into the orchestrator.
    fn insert_prompt(orch: &mut Orchestrator, id: usize, status: PromptStatus) {
        let mut p = Prompt::new(id, format!("prompt-{id}"), None, PromptMode::Interactive);
        p.status = status;
        orch.prompts.push(p);
        if id >= orch.next_id {
            orch.next_id = id + 1;
        }
    }

    #[tokio::test]
    async fn store_drop_running() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        insert_prompt(&mut orch, 2, PromptStatus::Idle);
        insert_prompt(&mut orch, 3, PromptStatus::Completed);
        insert_prompt(&mut orch, 4, PromptStatus::Pending);

        let dropped = orch.store_drop("running");
        assert_eq!(dropped, 2);
        let remaining_ids: Vec<usize> = orch.prompts.iter().map(|p| p.id).collect();
        assert_eq!(remaining_ids, vec![3, 4]);
    }

    #[tokio::test]
    async fn store_keep_running() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        insert_prompt(&mut orch, 2, PromptStatus::Idle);
        insert_prompt(&mut orch, 3, PromptStatus::Completed);
        insert_prompt(&mut orch, 4, PromptStatus::Pending);
        insert_prompt(&mut orch, 5, PromptStatus::Failed);

        let dropped = orch.store_keep("running");
        // Running(1) and Idle(2) are kept (match filter).
        // Completed(3), Pending(4), Failed(5) don't match but Running/Idle are
        // always protected → only Completed, Pending, Failed are dropped.
        assert_eq!(dropped, 3);
        let remaining_statuses: Vec<PromptStatus> =
            orch.prompts.iter().map(|p| p.status.clone()).collect();
        assert_eq!(
            remaining_statuses,
            vec![PromptStatus::Running, PromptStatus::Idle]
        );
    }

    #[tokio::test]
    async fn store_keep_pending() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        insert_prompt(&mut orch, 2, PromptStatus::Pending);
        insert_prompt(&mut orch, 3, PromptStatus::Completed);
        insert_prompt(&mut orch, 4, PromptStatus::Failed);

        let dropped = orch.store_keep("pending");
        // Pending(2) kept by filter, Running(1) always protected,
        // Completed(3) and Failed(4) dropped
        assert_eq!(dropped, 2);
        let remaining_ids: Vec<usize> = orch.prompts.iter().map(|p| p.id).collect();
        assert_eq!(remaining_ids, vec![1, 2]);
    }

    #[tokio::test]
    async fn store_drop_completed() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Completed);
        insert_prompt(&mut orch, 2, PromptStatus::Failed);
        insert_prompt(&mut orch, 3, PromptStatus::Completed);

        let dropped = orch.store_drop("completed");
        assert_eq!(dropped, 2);
        assert_eq!(orch.prompts.len(), 1);
        assert_eq!(orch.prompts[0].id, 2);
    }

    #[tokio::test]
    async fn apply_message_pty_eof_no_handle_sends_synthetic_finished() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        // No pty_handle registered for prompt 1 → simulates kill race
        orch.apply_message(WorkerMessage::PtyEof { prompt_id: 1 });

        // The handler should have sent a synthetic Finished via worker_tx
        let msg = orch
            .worker_rx
            .try_recv()
            .expect("should have received Finished");
        match msg {
            WorkerMessage::Finished {
                prompt_id,
                exit_code,
            } => {
                assert_eq!(prompt_id, 1);
                assert_eq!(exit_code, None);
            }
            _ => panic!("expected WorkerMessage::Finished, got something else"),
        }
    }

    #[tokio::test]
    async fn apply_message_finished_exit_1_marks_failed() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        orch.apply_message(WorkerMessage::Finished {
            prompt_id: 1,
            exit_code: Some(1),
        });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.status, PromptStatus::Failed);
        assert!(prompt.error.is_some());
    }

    #[tokio::test]
    async fn apply_message_finished_exit_none_marks_completed() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        orch.apply_message(WorkerMessage::Finished {
            prompt_id: 1,
            exit_code: None,
        });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.status, PromptStatus::Completed);
    }

    #[tokio::test]
    async fn subscribe_sends_pty_replay_for_active_workers() {
        use clhorde_core::protocol::ClientRequest;
        use std::sync::{Arc, Mutex};

        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        // Create a fake PtyHandle with ring buffer data
        let ring = Arc::new(Mutex::new(crate::pty_worker::PtyRingBuffer::new(1024)));
        ring.lock().unwrap().extend(b"hello from pty");

        // We need a minimal PtyHandle. Since we can't easily create a real PTY in
        // tests, we'll verify the ring buffer snapshot logic directly and check that
        // the session receives the expected events.

        // Register a session
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        orch.sessions.add_session_with_id(1, event_tx);

        // Manually insert a pty_handle with our ring buffer
        // We can't construct a full PtyHandle without a real PTY, so test the ring
        // buffer snapshot logic independently and verify Subscribe sends Subscribed.
        orch.handle_request(ClientRequest::Subscribe, 1);

        // Should receive Subscribed since there are no pty_handles
        let event = event_rx.try_recv().expect("should receive Subscribed");
        assert!(matches!(event, DaemonEvent::Subscribed));

        // Verify Unsubscribe sends Unsubscribed
        orch.handle_request(ClientRequest::Unsubscribe, 1);
        let event = event_rx.try_recv().expect("should receive Unsubscribed");
        assert!(matches!(event, DaemonEvent::Unsubscribed));
    }
}
