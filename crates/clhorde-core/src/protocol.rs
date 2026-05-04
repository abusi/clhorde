//! IPC protocol message types for daemon <-> TUI/CLI communication.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current protocol version. Increment when making breaking changes to
/// `ClientRequest`, `DaemonEvent`, or `DaemonState`.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientRequest {
    SubmitPrompt {
        text: String,
        cwd: Option<String>,
        mode: String,
        worktree: bool,
        tags: Vec<String>,
        /// Prompt IDs (as known to the client) that must complete before this one runs.
        /// The daemon resolves these to UUIDs at submit time.
        #[serde(default)]
        depends_on: Vec<usize>,
        /// Optional shared worktree key. Multiple prompts with the same
        /// `worktree_id` and `worktree: true` reuse a single worktree.
        #[serde(default)]
        worktree_id: Option<String>,
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
    SetPromptMode {
        prompt_id: usize,
        mode: String,
    },
    /// Replace the dependency list of a Pending or Blocked prompt.
    /// IDs are resolved to UUIDs server-side.
    SetDependencies {
        prompt_id: usize,
        depends_on: Vec<usize>,
    },
    /// Write or remove a single annotation on a prompt. The daemon stores
    /// the value verbatim and never interprets it. Pass `Value::Null` to
    /// remove the key. Unknown `prompt_id` returns `DaemonEvent::Error`.
    SetAnnotation {
        prompt_id: usize,
        key: String,
        value: Value,
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
    /// Replay of PTY ring buffer bytes for late-joining clients.
    PtyReplay {
        prompt_id: usize,
        data: Vec<u8>,
    },
    /// Acknowledgement that subscription is active — PTY byte forwarding enabled.
    Subscribed,
    /// Acknowledgement that subscription is inactive — PTY byte forwarding disabled.
    Unsubscribed,
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
    /// UUIDs of prompts this one depends on.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Subset of `depends_on` that is not yet Completed (computed by the daemon).
    /// Empty means the prompt is dispatchable.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Shared worktree key, if any.
    #[serde(default)]
    pub worktree_id: Option<String>,
    /// Opaque annotations attached by clients. The daemon does not interpret
    /// these — see `Prompt::annotations`.
    #[serde(default)]
    pub annotations: BTreeMap<String, Value>,
}

impl PromptInfo {
    /// Parse the `status` string back to a `PromptStatus` enum.
    pub fn status_enum(&self) -> crate::prompt::PromptStatus {
        match self.status.as_str() {
            "Pending" => crate::prompt::PromptStatus::Pending,
            "Blocked" => crate::prompt::PromptStatus::Blocked,
            "Running" => crate::prompt::PromptStatus::Running,
            "Idle" => crate::prompt::PromptStatus::Idle,
            "Completed" => crate::prompt::PromptStatus::Completed,
            "Failed" => crate::prompt::PromptStatus::Failed,
            _ => crate::prompt::PromptStatus::Pending,
        }
    }

    /// Parse the `mode` string back to a `PromptMode` enum.
    pub fn mode_enum(&self) -> crate::prompt::PromptMode {
        crate::prompt::PromptMode::from_mode_str(&self.mode)
    }

    /// Format `elapsed_secs` as a human-readable duration string.
    pub fn elapsed_display(&self) -> Option<String> {
        self.elapsed_secs.map(crate::prompt::format_duration)
    }

    /// Return the status emoji symbol.
    pub fn status_symbol(&self) -> &'static str {
        match self.status.as_str() {
            "Pending" => "⏳",
            "Blocked" => "🔒",
            "Running" => "🔄",
            "Idle" => "💬",
            "Completed" => "✅",
            "Failed" => "❌",
            _ => "⏳",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub prompts: Vec<PromptInfo>,
    pub max_workers: usize,
    pub active_workers: usize,
    pub default_mode: String,
    /// Protocol version of the daemon. Clients should warn if mismatched.
    #[serde(default)]
    pub protocol_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_pty_replay() {
        let event = DaemonEvent::PtyReplay {
            prompt_id: 42,
            data: vec![0x1b, 0x5b, 0x48, 0x65, 0x6c, 0x6c, 0x6f],
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: DaemonEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            DaemonEvent::PtyReplay { prompt_id, data } => {
                assert_eq!(prompt_id, 42);
                assert_eq!(data, vec![0x1b, 0x5b, 0x48, 0x65, 0x6c, 0x6c, 0x6f]);
            }
            _ => panic!("expected PtyReplay"),
        }
    }

    #[test]
    fn serde_roundtrip_subscribed() {
        let event = DaemonEvent::Subscribed;
        let json = serde_json::to_string(&event).unwrap();
        let decoded: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonEvent::Subscribed));
    }

    #[test]
    fn serde_roundtrip_unsubscribed() {
        let event = DaemonEvent::Unsubscribed;
        let json = serde_json::to_string(&event).unwrap();
        let decoded: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, DaemonEvent::Unsubscribed));
    }

    #[test]
    fn protocol_version_is_1() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn daemon_state_serde_roundtrip_includes_version() {
        let state = DaemonState {
            prompts: vec![],
            max_workers: 3,
            active_workers: 0,
            default_mode: "interactive".to_string(),
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("protocol_version"));
        let decoded: DaemonState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn daemon_state_without_version_defaults_to_zero() {
        // Backward compat: old daemons without protocol_version field
        let json =
            r#"{"prompts":[],"max_workers":3,"active_workers":0,"default_mode":"interactive"}"#;
        let decoded: DaemonState = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.protocol_version, 0);
    }
}
