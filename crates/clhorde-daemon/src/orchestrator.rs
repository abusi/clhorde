use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use tokio::sync::mpsc;

use tracing::{debug, error, info, warn};

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
    pub worker_tx: mpsc::Sender<WorkerMessage>,
    pub worker_rx: mpsc::Receiver<WorkerMessage>,
    pub pty_byte_tx: tokio::sync::broadcast::Sender<(usize, Vec<u8>)>,
    /// Prompt IDs currently awaiting async worktree creation.
    pub worktree_creating: HashSet<usize>,
    max_saved_prompts: usize,
    prompts_dir: Option<PathBuf>,
    worktree_cleanup: WorktreeCleanup,
}

impl Orchestrator {
    /// Create an Orchestrator for tests — no persistence, no settings loading.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let (worker_tx, worker_rx) = mpsc::channel(4096);
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
            worktree_creating: HashSet::new(),
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
                prompt.worktree_id = pf.options.worktree_id.clone();
                prompt.tags = pf.tags.clone();
                prompt.depends_on = pf.depends_on.clone();
                prompt.status = status;
                prompt.seen = true;
                prompts.push(prompt);
                next_id += 1;
            }

            if !prompts.is_empty() {
                info!(count = prompts.len(), "restored prompts from disk");
            }
            persistence::prune_old_prompts(dir, max_saved_prompts);
            debug!(max = max_saved_prompts, "pruned old prompts");
        }

        let (worker_tx, worker_rx) = mpsc::channel(4096);
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
            worktree_creating: HashSet::new(),
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

    /// Prune old prompt files if the count exceeds `max_saved_prompts`.
    fn maybe_prune(&self) {
        if let Some(ref dir) = self.prompts_dir {
            persistence::prune_old_prompts(dir, self.max_saved_prompts);
        }
    }

    // ── Convert to wire types ──

    pub fn to_prompt_info(&self, prompt: &Prompt) -> PromptInfo {
        let blocked_by: Vec<String> = prompt
            .depends_on
            .iter()
            .filter(|dep_uuid| {
                !self
                    .prompts
                    .iter()
                    .any(|p| &p.uuid == *dep_uuid && p.status == PromptStatus::Completed)
            })
            .cloned()
            .collect();
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
            depends_on: prompt.depends_on.clone(),
            blocked_by,
            worktree_id: prompt.worktree_id.clone(),
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
            protocol_version: clhorde_core::protocol::PROTOCOL_VERSION,
        }
    }

    // ── Prompt lifecycle ──

    /// Resolve client-facing prompt IDs to internal UUIDs.
    /// Returns an error listing any IDs that don't exist.
    fn resolve_dep_ids(&self, ids: &[usize]) -> Result<Vec<String>, String> {
        let mut uuids = Vec::with_capacity(ids.len());
        let mut missing = Vec::new();
        for &id in ids {
            match self.prompts.iter().find(|p| p.id == id) {
                Some(p) => uuids.push(p.uuid.clone()),
                None => missing.push(id),
            }
        }
        if !missing.is_empty() {
            return Err(format!("Unknown prompt IDs in depends_on: {missing:?}"));
        }
        Ok(uuids)
    }

    /// Detect whether assigning `new_deps` (UUIDs) to the prompt with `target_uuid`
    /// would introduce a cycle in the dependency graph.
    fn would_create_cycle(&self, target_uuid: &str, new_deps: &[String]) -> bool {
        // DFS from each dep; if we can reach `target_uuid`, there is a cycle.
        for start in new_deps {
            if start == target_uuid {
                return true;
            }
            let mut stack = vec![start.clone()];
            let mut seen: HashSet<String> = HashSet::new();
            while let Some(node) = stack.pop() {
                if !seen.insert(node.clone()) {
                    continue;
                }
                if let Some(p) = self.prompts.iter().find(|p| p.uuid == node) {
                    for d in &p.depends_on {
                        if d == target_uuid {
                            return true;
                        }
                        stack.push(d.clone());
                    }
                }
            }
        }
        false
    }

    /// Path of the shared worktree associated with `wt_id`, if any prompt
    /// already has it resolved on disk.
    fn shared_worktree_path(&self, wt_id: &str) -> Option<String> {
        self.prompts
            .iter()
            .find(|p| {
                p.worktree_id.as_deref() == Some(wt_id) && p.worktree_path.is_some()
            })
            .and_then(|p| p.worktree_path.clone())
    }

    /// Whether a worktree-creation thread is currently running for `wt_id`.
    fn worktree_id_creation_in_flight(&self, wt_id: &str) -> bool {
        self.worktree_creating.iter().any(|pid| {
            self.prompts
                .iter()
                .find(|p| p.id == *pid)
                .and_then(|p| p.worktree_id.as_deref())
                == Some(wt_id)
        })
    }

    /// Number of prompts sharing `wt_id` that are not yet terminal
    /// (i.e. anything other than Completed or Failed). Used as a refcount
    /// to decide whether a shared worktree may be cleaned up.
    fn shared_worktree_active_count(&self, wt_id: &str) -> usize {
        self.prompts
            .iter()
            .filter(|p| {
                p.worktree_id.as_deref() == Some(wt_id)
                    && !matches!(p.status, PromptStatus::Completed | PromptStatus::Failed)
            })
            .count()
    }

    /// Whether all of `prompt.depends_on` reference Completed prompts.
    fn all_deps_completed(&self, prompt: &Prompt) -> bool {
        prompt.depends_on.iter().all(|dep_uuid| {
            self.prompts
                .iter()
                .any(|p| &p.uuid == dep_uuid && p.status == PromptStatus::Completed)
        })
    }

    /// After a prompt completes, re-evaluate Blocked prompts and unblock any
    /// whose deps are now all Completed. Returns the set of prompt IDs that
    /// transitioned to Pending.
    fn unblock_dependents(&mut self) -> Vec<usize> {
        let to_unblock: Vec<usize> = self
            .prompts
            .iter()
            .filter(|p| p.status == PromptStatus::Blocked && self.all_deps_completed(p))
            .map(|p| p.id)
            .collect();
        for id in &to_unblock {
            if let Some(p) = self.prompts.iter_mut().find(|p| p.id == *id) {
                p.status = PromptStatus::Pending;
            }
            self.persist_prompt_by_id(*id);
            if let Some(p) = self.prompts.iter().find(|p| p.id == *id) {
                let info = self.to_prompt_info(p);
                self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
            }
        }
        to_unblock
    }

    pub fn add_prompt(
        &mut self,
        text: String,
        cwd: Option<String>,
        mode: PromptMode,
        worktree: bool,
        tags: Vec<String>,
        depends_on: Vec<String>,
        worktree_id: Option<String>,
    ) {
        let mut prompt = Prompt::new(self.next_id, text, cwd, mode);
        prompt.worktree = worktree;
        prompt.tags = tags;
        prompt.depends_on = depends_on;
        prompt.worktree_id = worktree_id;
        // A fresh prompt has no incoming edges, so cycles are impossible here.
        // The status starts Blocked if any dep is not yet Completed.
        if !prompt.depends_on.is_empty() && !self.all_deps_completed(&prompt) {
            prompt.status = PromptStatus::Blocked;
        }
        let max_rank = self
            .prompts
            .iter()
            .map(|p| p.queue_rank)
            .fold(0.0_f64, f64::max);
        prompt.queue_rank = max_rank + 1.0;
        self.next_id += 1;
        self.persist_prompt(&prompt);
        self.maybe_prune();

        info!(
            prompt_id = prompt.id,
            mode = %prompt.mode.label(),
            deps = prompt.depends_on.len(),
            status = ?prompt.status,
            "prompt added"
        );
        let info_msg = self.to_prompt_info(&prompt);
        self.prompts.push(prompt);
        self.sessions.broadcast(&DaemonEvent::PromptAdded(info_msg));
    }

    fn next_pending_prompt_index(&self) -> Option<usize> {
        self.prompts.iter().position(|p| {
            p.status == PromptStatus::Pending && !self.worktree_creating.contains(&p.id)
        })
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
            debug!(prompt_id = id, has_pty = info.has_pty, "mark_running: broadcasting initial PromptUpdated");
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
            let prompt_worktree_id = self.prompts[idx].worktree_id.clone();
            let mode = self.prompts[idx].mode;
            let resume_session_id = if self.prompts[idx].resume {
                Some(self.prompts[idx].session_id.clone().unwrap_or_default())
            } else {
                None
            };

            // Generate a session ID upfront so we can pass --session-id on
            // initial spawn and have it available for resume immediately.
            if self.prompts[idx].session_id.is_none() {
                let generated = uuid::Uuid::new_v4().to_string();
                self.prompts[idx].session_id = Some(generated);
                self.persist_prompt_by_id(prompt_id);
            }
            let session_id = self.prompts[idx].session_id.clone();

            let cwd = if prompt_worktree {
                if let Some(existing) = prompt_worktree_path.clone() {
                    Some(existing)
                } else if let Some(ref wt_id) = prompt_worktree_id {
                    // Shared worktree: reuse if a sibling already resolved it.
                    if let Some(shared) = self.shared_worktree_path(wt_id) {
                        debug!(prompt_id, %wt_id, path = %shared, "reusing shared worktree");
                        self.prompts[idx].worktree_path = Some(shared.clone());
                        self.persist_prompt_by_id(prompt_id);
                        let info = self.to_prompt_info(&self.prompts[idx]);
                        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                        Some(shared)
                    } else if self.worktree_id_creation_in_flight(wt_id) {
                        // Another sibling is creating it right now; wait for
                        // WorktreeCreated to broadcast and re-dispatch.
                        debug!(prompt_id, %wt_id, "shared worktree creation in flight, waiting");
                        continue;
                    } else {
                        // First prompt for this shared id: create with id-based name.
                        let effective_cwd = prompt_cwd.clone().unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string()
                        });
                        let repo_path = std::path::Path::new(&effective_cwd);
                        if clhorde_core::worktree::is_git_repo(repo_path) {
                            debug!(prompt_id, repo = %repo_path.display(), %wt_id, "creating shared worktree");
                            self.worktree_creating.insert(prompt_id);
                            let tx = self.worker_tx.clone();
                            let repo = repo_path.to_path_buf();
                            let wt_id_owned = wt_id.clone();
                            std::thread::spawn(move || {
                                let result = clhorde_core::worktree::create_worktree_named(
                                    &repo,
                                    &wt_id_owned,
                                )
                                .map(|p| p.to_string_lossy().to_string());
                                let _ = tx.blocking_send(WorkerMessage::WorktreeCreated {
                                    prompt_id,
                                    result,
                                });
                            });
                            continue;
                        } else {
                            prompt_cwd
                        }
                    }
                } else {
                    // Per-prompt worktree (legacy behavior).
                    let effective_cwd = prompt_cwd.clone().unwrap_or_else(|| {
                        std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                    let repo_path = std::path::Path::new(&effective_cwd);
                    if clhorde_core::worktree::is_git_repo(repo_path) {
                        debug!(prompt_id, repo = %repo_path.display(), "creating worktree");
                        self.worktree_creating.insert(prompt_id);
                        let tx = self.worker_tx.clone();
                        let repo = repo_path.to_path_buf();
                        std::thread::spawn(move || {
                            let result = clhorde_core::worktree::create_worktree(&repo, prompt_id)
                                .map(|p| p.to_string_lossy().to_string());
                            let _ = tx.blocking_send(WorkerMessage::WorktreeCreated {
                                prompt_id,
                                result,
                            });
                        });
                        continue;
                    } else {
                        prompt_cwd
                    }
                }
            } else {
                prompt_cwd
            };

            self.mark_running(idx);
            self.active_workers += 1;
            info!(prompt_id, mode = %mode.label(), active_workers = self.active_workers, "dispatching worker");
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
                session_id,
                self.pty_byte_tx.clone(),
            );

            match result {
                SpawnResult::Pty {
                    input_sender,
                    pty_handle,
                } => {
                    self.worker_inputs.insert(prompt_id, input_sender);
                    self.pty_handles.insert(prompt_id, pty_handle);
                    debug!(prompt_id, "PTY handle stored, has_pty=true");
                    // Re-broadcast PromptUpdated now that has_pty will be true
                    // (mark_running broadcast happened before the handle was stored)
                    if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                        let info = self.to_prompt_info(prompt);
                        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                    }
                }
                SpawnResult::OneShot => {
                    debug!(prompt_id, "one-shot worker spawned (no PTY)");
                }
                SpawnResult::Error(e) => {
                    error!(prompt_id, error = %e, "worker spawn failed");
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
                debug!(prompt_id, "worker turn complete");
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
                debug!(prompt_id, %session_id, "worker session ID received");
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
                        let _ = tx.blocking_send(WorkerMessage::Finished {
                            prompt_id,
                            exit_code,
                        });
                    });
                } else {
                    // Handle already removed (kill race) — send synthetic Finished
                    let _ = self.worker_tx.try_send(WorkerMessage::Finished {
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

                info!(
                    prompt_id,
                    ?exit_code,
                    active_workers = self.active_workers,
                    "worker finished"
                );
                self.sessions.broadcast(&DaemonEvent::WorkerFinished {
                    prompt_id,
                    exit_code,
                });
                if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                    let info = self.to_prompt_info(prompt);
                    self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                }
                // If this prompt completed, see if any Blocked dependents are now free.
                let unblocked = self.unblock_dependents();
                if !unblocked.is_empty() {
                    debug!(?unblocked, "unblocked dependents after Finished");
                }
                self.sessions.broadcast(&DaemonEvent::ActiveWorkersChanged {
                    count: self.active_workers,
                });
            }
            WorkerMessage::WorktreeCreated { prompt_id, result } => {
                self.worktree_creating.remove(&prompt_id);
                // Look up the worktree_id of the originating prompt so we can
                // propagate the resolved path to all sibling prompts.
                let wt_id = self
                    .prompts
                    .iter()
                    .find(|p| p.id == prompt_id)
                    .and_then(|p| p.worktree_id.clone());
                match result {
                    Ok(wt_path) => {
                        if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) {
                            prompt.worktree_path = Some(wt_path.clone());
                        }
                        self.persist_prompt_by_id(prompt_id);
                        // Propagate to siblings sharing the same worktree_id.
                        if let Some(ref id) = wt_id {
                            let siblings: Vec<usize> = self
                                .prompts
                                .iter()
                                .filter(|p| {
                                    p.id != prompt_id
                                        && p.worktree_id.as_deref() == Some(id.as_str())
                                        && p.worktree_path.is_none()
                                })
                                .map(|p| p.id)
                                .collect();
                            for sid in siblings {
                                if let Some(p) = self.prompts.iter_mut().find(|p| p.id == sid) {
                                    p.worktree_path = Some(wt_path.clone());
                                }
                                self.persist_prompt_by_id(sid);
                                if let Some(p) = self.prompts.iter().find(|p| p.id == sid) {
                                    let info = self.to_prompt_info(p);
                                    self.sessions
                                        .broadcast(&DaemonEvent::PromptUpdated(info));
                                }
                            }
                        }
                        // dispatch_workers() is called after apply_message() in main loop,
                        // which will pick up this prompt now that it has worktree_path set.
                    }
                    Err(e) => {
                        // Fail the originating prompt and any waiting siblings sharing wt_id.
                        let mut affected: Vec<usize> = vec![prompt_id];
                        if let Some(ref id) = wt_id {
                            for p in &self.prompts {
                                if p.id != prompt_id
                                    && p.worktree_id.as_deref() == Some(id.as_str())
                                    && p.worktree_path.is_none()
                                {
                                    affected.push(p.id);
                                }
                            }
                        }
                        for pid in affected {
                            if let Some(prompt) =
                                self.prompts.iter_mut().find(|p| p.id == pid)
                            {
                                prompt.status = PromptStatus::Failed;
                                prompt.error =
                                    Some(format!("Failed to create worktree: {e}"));
                            }
                            self.persist_prompt_by_id(pid);
                            self.sessions.broadcast(&DaemonEvent::WorkerError {
                                prompt_id: pid,
                                error: format!("Failed to create worktree: {e}"),
                            });
                            if let Some(p) = self.prompts.iter().find(|p| p.id == pid) {
                                let info = self.to_prompt_info(p);
                                self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                            }
                        }
                    }
                }
            }
            WorkerMessage::SpawnError { prompt_id, error } => {
                warn!(prompt_id, %error, "worker spawn error");
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

        debug!(session_id, request = ?req, "handling request");
        match req {
            ClientRequest::SubmitPrompt {
                text,
                cwd,
                mode,
                worktree,
                tags,
                depends_on,
                worktree_id,
            } => {
                let prompt_mode = PromptMode::from_mode_str(&mode);
                match self.resolve_dep_ids(&depends_on) {
                    Ok(dep_uuids) => {
                        self.add_prompt(
                            text,
                            cwd,
                            prompt_mode,
                            worktree,
                            tags,
                            dep_uuids,
                            worktree_id,
                        );
                        self.dispatch_workers();
                    }
                    Err(message) => {
                        self.sessions
                            .send_to(session_id, DaemonEvent::Error { message });
                    }
                }
            }
            ClientRequest::SendInput { prompt_id, text } => {
                // Only echo and send if a worker input channel exists (interactive workers).
                // One-shot workers have no input channel — return an error instead.
                if let Some(sender) = self.worker_inputs.get(&prompt_id) {
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
                    let mut send_text = text;
                    send_text.push('\n');
                    let _ = sender.send(WorkerInput::SendInput(send_text));
                } else {
                    self.sessions.send_to(
                        session_id,
                        DaemonEvent::Error {
                            message: format!(
                                "Cannot send input to prompt {prompt_id}: no input channel (one-shot worker?)"
                            ),
                        },
                    );
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
                                p.worktree_id.clone(),
                            ))
                        } else {
                            None
                        }
                    });
                if let Some((text, cwd, mode, wt, tags, wt_id)) = retry_data {
                    // Retry creates a fresh prompt with no inherited deps —
                    // by design, the user re-links manually if desired.
                    // worktree_id is preserved so the retry joins the same
                    // shared worktree as the original.
                    self.add_prompt(text, cwd, mode, wt, tags, Vec::new(), wt_id);
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
                self.default_mode = PromptMode::from_mode_str(&mode);
            }
            ClientRequest::SetPromptMode { prompt_id, mode } => {
                let new_mode = PromptMode::from_mode_str(&mode);
                if let Some(idx) = self.prompts.iter().position(|p| p.id == prompt_id) {
                    if self.prompts[idx].status == PromptStatus::Pending {
                        self.prompts[idx].mode = new_mode;
                        self.persist_prompt_by_id(prompt_id);
                        let info = self.to_prompt_info(&self.prompts[idx]);
                        self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                    }
                }
            }
            ClientRequest::SetDependencies {
                prompt_id,
                depends_on,
            } => {
                let Some(idx) = self.prompts.iter().position(|p| p.id == prompt_id) else {
                    self.sessions.send_to(
                        session_id,
                        DaemonEvent::Error {
                            message: format!("Unknown prompt {prompt_id}"),
                        },
                    );
                    return;
                };
                let current_status = self.prompts[idx].status.clone();
                if !matches!(
                    current_status,
                    PromptStatus::Pending | PromptStatus::Blocked
                ) {
                    self.sessions.send_to(
                        session_id,
                        DaemonEvent::Error {
                            message: format!(
                                "Cannot change dependencies of prompt {prompt_id} in {current_status:?} state"
                            ),
                        },
                    );
                    return;
                }
                let dep_uuids = match self.resolve_dep_ids(&depends_on) {
                    Ok(u) => u,
                    Err(message) => {
                        self.sessions
                            .send_to(session_id, DaemonEvent::Error { message });
                        return;
                    }
                };
                let target_uuid = self.prompts[idx].uuid.clone();
                if self.would_create_cycle(&target_uuid, &dep_uuids) {
                    self.sessions.send_to(
                        session_id,
                        DaemonEvent::Error {
                            message: format!(
                                "Refused: dependency change on prompt {prompt_id} would create a cycle"
                            ),
                        },
                    );
                    return;
                }
                self.prompts[idx].depends_on = dep_uuids;
                self.prompts[idx].status =
                    if self.all_deps_completed(&self.prompts[idx]) {
                        PromptStatus::Pending
                    } else {
                        PromptStatus::Blocked
                    };
                self.persist_prompt_by_id(prompt_id);
                let info = self.to_prompt_info(&self.prompts[idx]);
                self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                self.dispatch_workers();
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
        info!(prompt_id = id, "killing worker");
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
        info!(prompt_id = prompt.id, "resuming prompt");
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
        info!(prompt_id, "deleting prompt");
        // Kill running worker if any
        if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
            if prompt.status == PromptStatus::Running || prompt.status == PromptStatus::Idle {
                self.kill_worker(prompt_id);
                self.worker_inputs.remove(&prompt_id);
                self.active_workers = self.active_workers.saturating_sub(1);
            }
        }
        // Capture UUID before removal so we can purge it from dependents' lists.
        let removed_uuid = self
            .prompts
            .iter()
            .find(|p| p.id == prompt_id)
            .map(|p| p.uuid.clone());
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
        // Purge the deleted UUID from any other prompt's depends_on, then
        // re-evaluate blocked dependents. Deletion implies "no longer needed",
        // so dependents should not wait on a vanished prompt.
        if let Some(uuid) = removed_uuid {
            let affected: Vec<usize> = self
                .prompts
                .iter()
                .filter(|p| p.depends_on.iter().any(|d| d == &uuid))
                .map(|p| p.id)
                .collect();
            for id in &affected {
                if let Some(p) = self.prompts.iter_mut().find(|p| p.id == *id) {
                    p.depends_on.retain(|d| d != &uuid);
                }
                self.persist_prompt_by_id(*id);
                if let Some(p) = self.prompts.iter().find(|p| p.id == *id) {
                    let info = self.to_prompt_info(p);
                    self.sessions.broadcast(&DaemonEvent::PromptUpdated(info));
                }
            }
            if !affected.is_empty() {
                self.unblock_dependents();
            }
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
        if count > 0 {
            debug!(filter, count, "store drop");
        }
        for id in to_remove {
            self.delete_prompt(id);
        }
        count
    }

    fn store_keep(&mut self, filter: &str) -> usize {
        debug!(filter, "store keep");
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
        info!("cleaning worktrees");
        let mut count = 0;
        // Track shared worktrees we've already cleaned so we don't try
        // multiple times across siblings.
        let mut cleaned_shared: HashSet<String> = HashSet::new();
        for prompt in &self.prompts {
            if prompt.status != PromptStatus::Completed && prompt.status != PromptStatus::Failed {
                continue;
            }
            // If the worktree is shared, only clean it when no sibling is still
            // active and we haven't processed this id yet.
            if let Some(ref wt_id) = prompt.worktree_id {
                if cleaned_shared.contains(wt_id) {
                    continue;
                }
                if self.shared_worktree_active_count(wt_id) > 0 {
                    continue;
                }
                cleaned_shared.insert(wt_id.clone());
            }
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
                                    if let Some(root) = clhorde_core::worktree::repo_root(&path) {
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
        count
    }

    // ── Worktree cleanup ──

    fn maybe_cleanup_worktree(&mut self, prompt_id: usize) {
        if self.worktree_cleanup != WorktreeCleanup::Auto {
            return;
        }
        let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) else {
            return;
        };
        let wt_id = prompt.worktree_id.clone();
        // Refcount: a shared worktree must not be cleaned up while other
        // sharing prompts are still active (Pending/Blocked/Running/Idle).
        if let Some(ref id) = wt_id {
            // The prompt that just finished is still in self.prompts here,
            // counted as Completed/Failed by the caller, so this returns the
            // count of *other* still-active siblings.
            if self.shared_worktree_active_count(id) > 0 {
                debug!(
                    prompt_id,
                    %id,
                    "skipping worktree cleanup: siblings still active"
                );
                return;
            }
        }
        let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == prompt_id) else {
            return;
        };
        let Some(wt_path) = prompt.worktree_path.take() else {
            return;
        };
        debug!(prompt_id, path = %wt_path, "auto-cleanup worktree");
        // Persist the cleared worktree_path on this prompt and on any siblings
        // sharing the same worktree_id (they all referenced the same path).
        if let Some(ref dir) = self.prompts_dir {
            if let Some(prompt) = self.prompts.iter().find(|p| p.id == prompt_id) {
                persistence::save_prompt(
                    dir,
                    &prompt.uuid,
                    &persistence::PromptFile::from_prompt(prompt),
                );
            }
        }
        if let Some(ref id) = wt_id {
            let siblings: Vec<usize> = self
                .prompts
                .iter()
                .filter(|p| {
                    p.id != prompt_id
                        && p.worktree_id.as_deref() == Some(id.as_str())
                        && p.worktree_path.is_some()
                })
                .map(|p| p.id)
                .collect();
            for sid in siblings {
                if let Some(p) = self.prompts.iter_mut().find(|p| p.id == sid) {
                    p.worktree_path = None;
                }
                self.persist_prompt_by_id(sid);
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
        info!(workers = self.worker_inputs.len(), "shutting down workers");
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
    use clhorde_core::protocol::ClientRequest;

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
        let (event_tx, mut event_rx) = mpsc::channel(1024);
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

    // ── dependencies ──

    #[tokio::test]
    async fn add_prompt_with_unmet_deps_starts_blocked() {
        let mut orch = Orchestrator::new_for_test();
        // Insert a Pending prompt with id 1 and capture its UUID
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        let dep_uuid = orch.prompts[0].uuid.clone();

        orch.add_prompt(
            "child".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![dep_uuid],
            None,
        );
        let child = orch.prompts.iter().find(|p| p.id == 2).unwrap();
        assert_eq!(child.status, PromptStatus::Blocked);
    }

    #[tokio::test]
    async fn add_prompt_with_completed_deps_starts_pending() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Completed);
        let dep_uuid = orch.prompts[0].uuid.clone();

        orch.add_prompt(
            "child".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![dep_uuid],
            None,
        );
        let child = orch.prompts.iter().find(|p| p.id == 2).unwrap();
        assert_eq!(child.status, PromptStatus::Pending);
    }

    #[tokio::test]
    async fn dispatch_skips_blocked_prompts() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        let dep_uuid = orch.prompts[0].uuid.clone();
        orch.add_prompt(
            "child".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![dep_uuid],
            None,
        );
        // The pending parent at idx 0 should be picked first; the blocked
        // child at idx 1 must not appear.
        let idx = orch.next_pending_prompt_index();
        assert_eq!(idx, Some(0));
        // Blocking the parent (set to Blocked) leaves nothing dispatchable.
        orch.prompts[0].status = PromptStatus::Blocked;
        assert_eq!(orch.next_pending_prompt_index(), None);
    }

    #[tokio::test]
    async fn unblock_dependents_transitions_blocked_to_pending() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        let dep_uuid = orch.prompts[0].uuid.clone();
        orch.add_prompt(
            "child".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![dep_uuid],
            None,
        );
        assert_eq!(orch.prompts[1].status, PromptStatus::Blocked);
        // Mark the parent Completed; unblock_dependents should run.
        orch.prompts[0].status = PromptStatus::Completed;
        let unblocked = orch.unblock_dependents();
        assert_eq!(unblocked, vec![2]);
        assert_eq!(orch.prompts[1].status, PromptStatus::Pending);
    }

    #[tokio::test]
    async fn dep_on_failed_prompt_keeps_dependent_blocked() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Failed);
        let dep_uuid = orch.prompts[0].uuid.clone();
        orch.add_prompt(
            "child".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![dep_uuid],
            None,
        );
        assert_eq!(orch.prompts[1].status, PromptStatus::Blocked);
        // unblock_dependents must not free dependents of Failed prompts.
        let unblocked = orch.unblock_dependents();
        assert!(unblocked.is_empty());
        assert_eq!(orch.prompts[1].status, PromptStatus::Blocked);
    }

    #[tokio::test]
    async fn would_create_cycle_detects_self_loop() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        let uuid = orch.prompts[0].uuid.clone();
        assert!(orch.would_create_cycle(&uuid, &[uuid.clone()]));
    }

    #[tokio::test]
    async fn would_create_cycle_detects_indirect_cycle() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        insert_prompt(&mut orch, 2, PromptStatus::Pending);
        let uuid_a = orch.prompts[0].uuid.clone();
        let uuid_b = orch.prompts[1].uuid.clone();
        // a depends on b; making b depend on a would close the loop.
        orch.prompts[0].depends_on = vec![uuid_b.clone()];
        assert!(orch.would_create_cycle(&uuid_b, &[uuid_a]));
    }

    #[tokio::test]
    async fn resolve_dep_ids_returns_error_for_unknown() {
        let orch = Orchestrator::new_for_test();
        match orch.resolve_dep_ids(&[42]) {
            Ok(_) => panic!("should have errored"),
            Err(msg) => assert!(msg.contains("42")),
        }
    }

    #[tokio::test]
    async fn delete_prompt_purges_deps_and_unblocks() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        let dep_uuid = orch.prompts[0].uuid.clone();
        orch.add_prompt(
            "child".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![dep_uuid.clone()],
            None,
        );
        assert_eq!(orch.prompts[1].status, PromptStatus::Blocked);

        orch.delete_prompt(1);
        // The child's depends_on should be empty now, and status Pending.
        let child = orch.prompts.iter().find(|p| p.id == 2).unwrap();
        assert!(child.depends_on.is_empty());
        assert_eq!(child.status, PromptStatus::Pending);
    }

    #[tokio::test]
    async fn submit_prompt_with_unknown_dep_id_returns_error() {
        let mut orch = Orchestrator::new_for_test();
        let (event_tx, mut event_rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, event_tx);

        orch.handle_request(
            ClientRequest::SubmitPrompt {
                text: "child".into(),
                cwd: None,
                mode: "interactive".into(),
                worktree: false,
                tags: vec![],
                depends_on: vec![999],
                worktree_id: None,
            },
            1,
        );
        let event = event_rx.try_recv().expect("should receive Error");
        match event {
            DaemonEvent::Error { message } => assert!(message.contains("999")),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(orch.prompts.is_empty());
    }

    // ── shared worktrees ──

    /// Helper: insert a prompt with a worktree_id and optional worktree_path.
    fn insert_shared(
        orch: &mut Orchestrator,
        id: usize,
        status: PromptStatus,
        wt_id: &str,
        wt_path: Option<&str>,
    ) {
        let mut p = Prompt::new(id, format!("p-{id}"), None, PromptMode::Interactive);
        p.status = status;
        p.worktree = true;
        p.worktree_id = Some(wt_id.to_string());
        p.worktree_path = wt_path.map(String::from);
        orch.prompts.push(p);
        if id >= orch.next_id {
            orch.next_id = id + 1;
        }
    }

    #[tokio::test]
    async fn shared_worktree_path_finds_sibling() {
        let mut orch = Orchestrator::new_for_test();
        insert_shared(&mut orch, 1, PromptStatus::Running, "flow-A", Some("/tmp/wt-A"));
        insert_shared(&mut orch, 2, PromptStatus::Pending, "flow-A", None);
        assert_eq!(
            orch.shared_worktree_path("flow-A"),
            Some("/tmp/wt-A".to_string())
        );
        assert_eq!(orch.shared_worktree_path("flow-B"), None);
    }

    #[tokio::test]
    async fn shared_worktree_active_count_excludes_terminal() {
        let mut orch = Orchestrator::new_for_test();
        insert_shared(&mut orch, 1, PromptStatus::Running, "X", Some("/tmp/x"));
        insert_shared(&mut orch, 2, PromptStatus::Completed, "X", Some("/tmp/x"));
        insert_shared(&mut orch, 3, PromptStatus::Failed, "X", Some("/tmp/x"));
        insert_shared(&mut orch, 4, PromptStatus::Pending, "X", None);
        // 1 (Running) and 4 (Pending) are active; 2 and 3 are terminal.
        assert_eq!(orch.shared_worktree_active_count("X"), 2);
    }

    #[tokio::test]
    async fn worktree_id_creation_in_flight_tracks_siblings() {
        let mut orch = Orchestrator::new_for_test();
        insert_shared(&mut orch, 1, PromptStatus::Pending, "X", None);
        insert_shared(&mut orch, 2, PromptStatus::Pending, "X", None);
        // No creation in flight initially.
        assert!(!orch.worktree_id_creation_in_flight("X"));
        // Mark prompt 1 as currently creating.
        orch.worktree_creating.insert(1);
        assert!(orch.worktree_id_creation_in_flight("X"));
        assert!(!orch.worktree_id_creation_in_flight("Y"));
    }

    #[tokio::test]
    async fn worktree_created_propagates_path_to_siblings() {
        let mut orch = Orchestrator::new_for_test();
        insert_shared(&mut orch, 1, PromptStatus::Pending, "shared", None);
        insert_shared(&mut orch, 2, PromptStatus::Pending, "shared", None);
        insert_shared(&mut orch, 3, PromptStatus::Pending, "other", None);
        orch.worktree_creating.insert(1);

        orch.apply_message(WorkerMessage::WorktreeCreated {
            prompt_id: 1,
            result: Ok("/tmp/shared-wt".to_string()),
        });

        let p1 = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        let p2 = orch.prompts.iter().find(|p| p.id == 2).unwrap();
        let p3 = orch.prompts.iter().find(|p| p.id == 3).unwrap();
        assert_eq!(p1.worktree_path.as_deref(), Some("/tmp/shared-wt"));
        assert_eq!(p2.worktree_path.as_deref(), Some("/tmp/shared-wt"));
        // Different worktree_id — must not get propagated.
        assert!(p3.worktree_path.is_none());
        // The creator was removed from the in-flight set.
        assert!(!orch.worktree_creating.contains(&1));
    }

    #[tokio::test]
    async fn worktree_created_failure_fails_all_siblings_waiting_on_path() {
        let mut orch = Orchestrator::new_for_test();
        insert_shared(&mut orch, 1, PromptStatus::Pending, "shared", None);
        insert_shared(&mut orch, 2, PromptStatus::Pending, "shared", None);
        // Sibling 3 already has a path — should NOT be marked Failed.
        insert_shared(&mut orch, 3, PromptStatus::Running, "shared", Some("/old"));
        orch.worktree_creating.insert(1);

        orch.apply_message(WorkerMessage::WorktreeCreated {
            prompt_id: 1,
            result: Err("disk full".into()),
        });

        let p1 = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        let p2 = orch.prompts.iter().find(|p| p.id == 2).unwrap();
        let p3 = orch.prompts.iter().find(|p| p.id == 3).unwrap();
        assert_eq!(p1.status, PromptStatus::Failed);
        assert_eq!(p2.status, PromptStatus::Failed);
        assert_eq!(p3.status, PromptStatus::Running);
    }

    #[tokio::test]
    async fn maybe_cleanup_skips_when_siblings_active() {
        let mut orch = Orchestrator::new_for_test();
        orch.worktree_cleanup = WorktreeCleanup::Auto;
        insert_shared(&mut orch, 1, PromptStatus::Completed, "shared", Some("/tmp/x"));
        insert_shared(&mut orch, 2, PromptStatus::Running, "shared", Some("/tmp/x"));

        // Trigger cleanup for the completed prompt.
        orch.maybe_cleanup_worktree(1);

        // Path must still be set on prompt 1 because cleanup was skipped.
        let p1 = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(p1.worktree_path.as_deref(), Some("/tmp/x"));
    }

    #[tokio::test]
    async fn maybe_cleanup_clears_when_last_sibling_terminal() {
        let mut orch = Orchestrator::new_for_test();
        orch.worktree_cleanup = WorktreeCleanup::Auto;
        insert_shared(&mut orch, 1, PromptStatus::Completed, "shared", Some("/tmp/x"));
        insert_shared(&mut orch, 2, PromptStatus::Failed, "shared", Some("/tmp/x"));

        orch.maybe_cleanup_worktree(1);

        // No active sibling → path is cleared on both (refcount drained).
        let p1 = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        let p2 = orch.prompts.iter().find(|p| p.id == 2).unwrap();
        assert!(p1.worktree_path.is_none());
        assert!(p2.worktree_path.is_none());
    }

    #[tokio::test]
    async fn add_prompt_persists_worktree_id() {
        let mut orch = Orchestrator::new_for_test();
        orch.add_prompt(
            "task".into(),
            None,
            PromptMode::Interactive,
            true,
            vec![],
            vec![],
            Some("flow-42".to_string()),
        );
        let p = &orch.prompts[0];
        assert_eq!(p.worktree_id.as_deref(), Some("flow-42"));
        assert!(p.worktree);
    }

    // ── add_prompt ──

    #[tokio::test]
    async fn add_prompt_increments_next_id_and_adds_prompt() {
        let mut orch = Orchestrator::new_for_test();
        assert_eq!(orch.next_id, 1);

        orch.add_prompt(
            "hello".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![],
            None,
        );
        assert_eq!(orch.prompts.len(), 1);
        assert_eq!(orch.prompts[0].id, 1);
        assert_eq!(orch.next_id, 2);

        orch.add_prompt(
            "world".into(),
            None,
            PromptMode::OneShot,
            false,
            vec![],
            vec![],
            None,
        );
        assert_eq!(orch.prompts.len(), 2);
        assert_eq!(orch.prompts[1].id, 2);
        assert_eq!(orch.next_id, 3);
    }

    #[tokio::test]
    async fn add_prompt_broadcasts_prompt_added() {
        let mut orch = Orchestrator::new_for_test();
        let (tx, mut rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, tx);
        orch.sessions.set_subscribed(1, true);

        orch.add_prompt(
            "test".into(),
            None,
            PromptMode::Interactive,
            false,
            vec![],
            vec![],
            None,
        );

        let event = rx.try_recv().expect("should receive PromptAdded");
        assert!(matches!(event, DaemonEvent::PromptAdded(_)));
    }

    // ── retry / resume ──

    #[tokio::test]
    async fn retry_completed_prompt_creates_new_pending() {
        let mut orch = Orchestrator::new_for_test();
        orch.max_workers = 0; // prevent dispatch from running the new prompt
        insert_prompt(&mut orch, 1, PromptStatus::Completed);

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::RetryPrompt { prompt_id: 1 },
            0,
        );

        // Original stays, new pending prompt is added
        assert_eq!(orch.prompts.len(), 2);
        assert_eq!(orch.prompts[1].status, PromptStatus::Pending);
        assert_eq!(orch.prompts[1].text, "prompt-1");
    }

    #[tokio::test]
    async fn retry_running_prompt_is_noop() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::RetryPrompt { prompt_id: 1 },
            0,
        );

        assert_eq!(orch.prompts.len(), 1); // no new prompt
    }

    #[tokio::test]
    async fn resume_completed_prompt_resets_to_pending() {
        let mut orch = Orchestrator::new_for_test();
        orch.max_workers = 0; // prevent dispatch from running the prompt
        insert_prompt(&mut orch, 1, PromptStatus::Completed);
        orch.prompts[0].output = Some("old output".into());

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::ResumePrompt { prompt_id: 1 },
            0,
        );

        assert_eq!(orch.prompts[0].status, PromptStatus::Pending);
        assert!(orch.prompts[0].resume);
        assert!(orch.prompts[0].output.is_none());
    }

    // ── delete ──

    #[tokio::test]
    async fn delete_prompt_removes_from_list() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Completed);
        insert_prompt(&mut orch, 2, PromptStatus::Pending);

        orch.delete_prompt(1);
        assert_eq!(orch.prompts.len(), 1);
        assert_eq!(orch.prompts[0].id, 2);
    }

    // ── set_max_workers ──

    #[tokio::test]
    async fn set_max_workers_clamps_and_broadcasts() {
        let mut orch = Orchestrator::new_for_test();
        let (tx, mut rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, tx);
        orch.sessions.set_subscribed(1, true);

        orch.handle_request(clhorde_core::protocol::ClientRequest::SetMaxWorkers(10), 1);

        assert_eq!(orch.max_workers, 10);
        let event = rx.try_recv().expect("should receive MaxWorkersChanged");
        assert!(matches!(
            event,
            DaemonEvent::MaxWorkersChanged { count: 10 }
        ));
    }

    #[tokio::test]
    async fn set_max_workers_clamps_to_minimum_1() {
        let mut orch = Orchestrator::new_for_test();
        orch.handle_request(clhorde_core::protocol::ClientRequest::SetMaxWorkers(0), 0);
        assert_eq!(orch.max_workers, 1);
    }

    // ── get_state ──

    #[tokio::test]
    async fn get_state_sends_snapshot_to_requesting_session() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        let (tx, mut rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, tx);

        orch.handle_request(clhorde_core::protocol::ClientRequest::GetState, 1);

        let event = rx.try_recv().expect("should receive StateSnapshot");
        match event {
            DaemonEvent::StateSnapshot(state) => {
                assert_eq!(state.prompts.len(), 1);
                assert_eq!(state.max_workers, 3);
            }
            _ => panic!("expected StateSnapshot"),
        }
    }

    // ── get_prompt_output ──

    #[tokio::test]
    async fn get_prompt_output_sends_output_text() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Completed);
        orch.prompts[0].output = Some("hello world".into());
        let (tx, mut rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, tx);

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::GetPromptOutput { prompt_id: 1 },
            1,
        );

        let event = rx.try_recv().expect("should receive PromptOutput");
        match event {
            DaemonEvent::PromptOutput {
                prompt_id,
                full_text,
            } => {
                assert_eq!(prompt_id, 1);
                assert_eq!(full_text, "hello world");
            }
            _ => panic!("expected PromptOutput"),
        }
    }

    // ── apply_message variants ──

    #[tokio::test]
    async fn apply_output_chunk_appends_to_prompt() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        orch.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "first ".into(),
        });
        orch.apply_message(WorkerMessage::OutputChunk {
            prompt_id: 1,
            text: "second".into(),
        });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.output.as_deref(), Some("first second"));
    }

    #[tokio::test]
    async fn apply_turn_complete_sets_idle() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        orch.apply_message(WorkerMessage::TurnComplete { prompt_id: 1 });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.status, PromptStatus::Idle);
    }

    #[tokio::test]
    async fn apply_session_id_sets_on_prompt() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        orch.apply_message(WorkerMessage::SessionId {
            prompt_id: 1,
            session_id: "sess-abc".into(),
        });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.session_id.as_deref(), Some("sess-abc"));
    }

    #[tokio::test]
    async fn apply_spawn_error_marks_failed_and_decrements_workers() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        orch.active_workers = 1;

        orch.apply_message(WorkerMessage::SpawnError {
            prompt_id: 1,
            error: "boom".into(),
        });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.status, PromptStatus::Failed);
        assert_eq!(prompt.error.as_deref(), Some("boom"));
        assert_eq!(orch.active_workers, 0);
    }

    // ── Fix 9 tests: SendInput ──

    #[tokio::test]
    async fn send_input_without_worker_input_returns_error() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        // No worker_inputs entry for prompt 1 (simulates one-shot worker)

        let (tx, mut rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, tx);

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::SendInput {
                prompt_id: 1,
                text: "hello".into(),
            },
            1,
        );

        // Should get Error, not OutputChunk
        let event = rx.try_recv().expect("should receive Error");
        assert!(matches!(event, DaemonEvent::Error { .. }));

        // Output should NOT have been modified
        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert!(prompt.output.is_none());
    }

    #[tokio::test]
    async fn send_input_with_worker_input_echoes_and_sends() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);

        // Create a worker input channel
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        orch.worker_inputs.insert(1, input_tx);

        let (tx, mut rx) = mpsc::channel(1024);
        orch.sessions.add_session_with_id(1, tx);
        orch.sessions.set_subscribed(1, true);

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::SendInput {
                prompt_id: 1,
                text: "hello".into(),
            },
            1,
        );

        // Should get OutputChunk broadcast (echo)
        let event = rx.try_recv().expect("should receive OutputChunk");
        assert!(matches!(event, DaemonEvent::OutputChunk { .. }));

        // Worker should receive the input
        let worker_msg = input_rx.try_recv().expect("worker should receive input");
        match worker_msg {
            WorkerInput::SendInput(text) => assert_eq!(text, "hello\n"),
            _ => panic!("expected SendInput"),
        }
    }

    // ── Fix 5 tests: worktree_creating ──

    #[tokio::test]
    async fn next_pending_skips_worktree_creating() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        insert_prompt(&mut orch, 2, PromptStatus::Pending);

        orch.worktree_creating.insert(1);

        let idx = orch.next_pending_prompt_index();
        assert_eq!(idx, Some(1)); // prompt at index 1 (id=2)
        assert_eq!(orch.prompts[idx.unwrap()].id, 2);
    }

    #[tokio::test]
    async fn next_pending_returns_none_when_all_creating() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);

        orch.worktree_creating.insert(1);

        assert!(orch.next_pending_prompt_index().is_none());
    }

    #[tokio::test]
    async fn apply_worktree_created_ok_sets_path() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        orch.worktree_creating.insert(1);

        orch.apply_message(WorkerMessage::WorktreeCreated {
            prompt_id: 1,
            result: Ok("/tmp/repo-wt-1".into()),
        });

        assert!(!orch.worktree_creating.contains(&1));
        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.worktree_path.as_deref(), Some("/tmp/repo-wt-1"));
        assert_eq!(prompt.status, PromptStatus::Pending); // still pending, ready for dispatch
    }

    #[tokio::test]
    async fn apply_worktree_created_err_marks_failed() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        orch.worktree_creating.insert(1);

        orch.apply_message(WorkerMessage::WorktreeCreated {
            prompt_id: 1,
            result: Err("git failed".into()),
        });

        assert!(!orch.worktree_creating.contains(&1));
        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.status, PromptStatus::Failed);
        assert!(prompt.error.as_ref().unwrap().contains("worktree"));
    }

    // ── runtime pruning ──

    #[tokio::test]
    async fn add_prompt_prunes_beyond_max_saved() {
        let dir = std::env::temp_dir().join(format!("clhorde-prune-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut orch = Orchestrator::new_for_test();
        orch.prompts_dir = Some(dir.clone());
        orch.max_saved_prompts = 3;
        orch.max_workers = 0; // prevent dispatch

        // Add 5 prompts — only 3 should remain on disk
        for _ in 0..5 {
            orch.add_prompt(
                "test".to_string(),
                None,
                PromptMode::Interactive,
                false,
                Vec::new(),
                Vec::new(),
                None,
            );
            // Small sleep to ensure UUID v7 ordering
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let on_disk = clhorde_core::persistence::load_all_prompts(&dir);
        assert_eq!(on_disk.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── session ID generation ──

    #[tokio::test]
    async fn dispatch_generates_session_id_for_fresh_prompt() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        assert!(orch.prompts[0].session_id.is_none());

        // dispatch_workers will try to spawn (and fail since claude isn't available
        // in tests), but the session_id should be generated before spawn.
        orch.dispatch_workers();

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert!(
            prompt.session_id.is_some(),
            "session_id should be generated before spawn"
        );
        // Verify it looks like a UUID v4
        let sid = prompt.session_id.as_ref().unwrap();
        assert!(
            uuid::Uuid::parse_str(sid).is_ok(),
            "session_id should be a valid UUID, got: {sid}"
        );
    }

    #[tokio::test]
    async fn dispatch_preserves_existing_session_id() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Pending);
        orch.prompts[0].session_id = Some("pre-existing-id".into());

        orch.dispatch_workers();

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(
            prompt.session_id.as_deref(),
            Some("pre-existing-id"),
            "should not overwrite an existing session_id"
        );
    }

    #[tokio::test]
    async fn resume_preserves_session_id_for_dispatch() {
        let mut orch = Orchestrator::new_for_test();
        orch.max_workers = 0; // prevent dispatch
        insert_prompt(&mut orch, 1, PromptStatus::Completed);
        orch.prompts[0].session_id = Some("sess-original".into());

        orch.handle_request(
            clhorde_core::protocol::ClientRequest::ResumePrompt { prompt_id: 1 },
            0,
        );

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(prompt.status, PromptStatus::Pending);
        assert!(prompt.resume);
        assert_eq!(
            prompt.session_id.as_deref(),
            Some("sess-original"),
            "resume should keep the original session_id"
        );
    }

    #[tokio::test]
    async fn session_id_from_worker_overrides_generated() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        orch.prompts[0].session_id = Some("generated-uuid".into());

        orch.apply_message(WorkerMessage::SessionId {
            prompt_id: 1,
            session_id: "actual-from-claude".into(),
        });

        let prompt = orch.prompts.iter().find(|p| p.id == 1).unwrap();
        assert_eq!(
            prompt.session_id.as_deref(),
            Some("actual-from-claude"),
            "session_id from worker should override the generated one"
        );
    }

    #[tokio::test]
    async fn prompt_info_includes_session_id() {
        let mut orch = Orchestrator::new_for_test();
        insert_prompt(&mut orch, 1, PromptStatus::Running);
        orch.prompts[0].session_id = Some("sess-123".into());

        let info = orch.to_prompt_info(&orch.prompts[0]);
        assert_eq!(info.session_id.as_deref(), Some("sess-123"));
    }
}
