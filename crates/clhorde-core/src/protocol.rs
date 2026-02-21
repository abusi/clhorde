//! IPC protocol message types for daemon <-> TUI/CLI communication.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientRequest {
    SubmitPrompt {
        text: String,
        cwd: Option<String>,
        mode: String,
        worktree: bool,
        tags: Vec<String>,
    },
    SendInput {
        prompt_id: usize,
        text: String,
    },
    SendBytes {
        prompt_id: usize,
        data: Vec<u8>,
    },
    KillWorker {
        prompt_id: usize,
    },
    RetryPrompt {
        prompt_id: usize,
    },
    ResumePrompt {
        prompt_id: usize,
    },
    DeletePrompt {
        prompt_id: usize,
    },
    MovePromptUp {
        prompt_id: usize,
    },
    MovePromptDown {
        prompt_id: usize,
    },
    SetMaxWorkers(usize),
    SetDefaultMode {
        mode: String,
    },
    GetState,
    GetPromptOutput {
        prompt_id: usize,
    },
    ResizePty {
        prompt_id: usize,
        cols: u16,
        rows: u16,
    },
    Subscribe,
    Unsubscribe,
    Ping,
    Shutdown,
    // Store commands
    StoreList,
    StoreCount,
    StorePath,
    StoreDrop {
        filter: String,
    },
    StoreKeep {
        filter: String,
    },
    CleanWorktrees,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonEvent {
    PromptAdded(PromptInfo),
    PromptUpdated(PromptInfo),
    PromptRemoved {
        prompt_id: usize,
    },
    OutputChunk {
        prompt_id: usize,
        text: String,
    },
    PromptOutput {
        prompt_id: usize,
        full_text: String,
    },
    PtyUpdate {
        prompt_id: usize,
    },
    WorkerStarted {
        prompt_id: usize,
    },
    WorkerFinished {
        prompt_id: usize,
        exit_code: Option<i32>,
    },
    WorkerError {
        prompt_id: usize,
        error: String,
    },
    TurnComplete {
        prompt_id: usize,
    },
    SessionId {
        prompt_id: usize,
        session_id: String,
    },
    MaxWorkersChanged {
        count: usize,
    },
    ActiveWorkersChanged {
        count: usize,
    },
    StateSnapshot(DaemonState),
    // Store responses
    StoreListResult {
        prompts: Vec<PromptInfo>,
    },
    StoreCountResult {
        pending: usize,
        running: usize,
        completed: usize,
        failed: usize,
    },
    StorePathResult {
        path: String,
    },
    StoreOpComplete {
        message: String,
    },
    Pong,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptInfo {
    pub id: usize,
    pub text: String,
    pub cwd: Option<String>,
    pub mode: String,
    pub status: String,
    pub output: Option<String>,
    pub error: Option<String>,
    pub worktree: bool,
    pub worktree_path: Option<String>,
    pub session_id: Option<String>,
    pub tags: Vec<String>,
    pub queue_rank: f64,
    pub seen: bool,
    pub resume: bool,
    pub output_len: usize,
    pub elapsed_secs: Option<f64>,
    pub uuid: String,
    pub has_pty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub prompts: Vec<PromptInfo>,
    pub max_workers: usize,
    pub active_workers: usize,
    pub default_mode: String,
}
