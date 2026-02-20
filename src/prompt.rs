use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PromptMode {
    Interactive,
    OneShot,
}

impl PromptMode {
    pub fn label(&self) -> &str {
        match self {
            PromptMode::Interactive => "interactive",
            PromptMode::OneShot => "one-shot",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            PromptMode::Interactive => PromptMode::OneShot,
            PromptMode::OneShot => PromptMode::Interactive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PromptStatus {
    Pending,
    Running,
    /// Turn complete, process alive, waiting for follow-up input.
    Idle,
    Completed,
    Failed,
}

impl PromptStatus {
    pub fn symbol(&self) -> &str {
        match self {
            PromptStatus::Pending => "⏳",
            PromptStatus::Running => "🔄",
            PromptStatus::Idle => "💬",
            PromptStatus::Completed => "✅",
            PromptStatus::Failed => "❌",
        }
    }
}

pub struct Prompt {
    pub id: usize,
    pub text: String,
    pub cwd: Option<String>,
    pub mode: PromptMode,
    pub status: PromptStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    /// Whether the user has seen/acknowledged this prompt's completion.
    pub seen: bool,
    /// Live PTY terminal state (only for running interactive/PTY workers).
    pub pty_state: Option<crate::pty_worker::SharedPtyState>,
    /// UUID v7 for persistence (unique file name).
    pub uuid: String,
    /// Ordering rank for persistence/restore.
    pub queue_rank: f64,
    /// Claude session ID (captured from stream-json init message).
    pub session_id: Option<String>,
    /// Whether this prompt should resume an existing claude session.
    pub resume: bool,
    /// Whether this prompt should run in a git worktree.
    pub worktree: bool,
    /// Path to the created worktree directory (for cleanup).
    pub worktree_path: Option<String>,
}

impl Prompt {
    pub fn new(id: usize, text: String, cwd: Option<String>, mode: PromptMode) -> Self {
        Self {
            id,
            text,
            cwd,
            mode,
            status: PromptStatus::Pending,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            seen: false,
            pty_state: None,
            uuid: uuid::Uuid::now_v7().to_string(),
            queue_rank: 0.0,
            session_id: None,
            resume: false,
            worktree: false,
            worktree_path: None,
        }
    }

    pub fn elapsed_secs(&self) -> Option<f64> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Instant::now);
        Some(end.duration_since(start).as_secs_f64())
    }

    /// Human-readable elapsed time, e.g. "4.2s", "2m 30s", "1h 5m".
    pub fn elapsed_display(&self) -> Option<String> {
        self.elapsed_secs().map(format_duration)
    }
}

/// Format seconds into a human-readable duration string.
/// - Under 60s: "4.2s"
/// - Under 1h: "2m 30s"
/// - 1h+: "1h 5m"
pub fn format_duration(secs: f64) -> String {
    let total = secs as u64;
    if total < 60 {
        format!("{secs:.1}s")
    } else if total < 3600 {
        let m = total / 60;
        let s = total % 60;
        format!("{m}m {s}s")
    } else {
        let h = total / 3600;
        let m = (total % 3600) / 60;
        format!("{h}h {m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── PromptMode ──

    #[test]
    fn toggle_interactive_to_oneshot() {
        assert_eq!(PromptMode::Interactive.toggle(), PromptMode::OneShot);
    }

    #[test]
    fn toggle_oneshot_to_interactive() {
        assert_eq!(PromptMode::OneShot.toggle(), PromptMode::Interactive);
    }

    #[test]
    fn toggle_roundtrip() {
        assert_eq!(PromptMode::Interactive.toggle().toggle(), PromptMode::Interactive);
    }

    #[test]
    fn label_interactive() {
        assert_eq!(PromptMode::Interactive.label(), "interactive");
    }

    #[test]
    fn label_oneshot() {
        assert_eq!(PromptMode::OneShot.label(), "one-shot");
    }

    // ── PromptStatus::symbol ──

    #[test]
    fn status_symbols() {
        assert_eq!(PromptStatus::Pending.symbol(), "⏳");
        assert_eq!(PromptStatus::Running.symbol(), "🔄");
        assert_eq!(PromptStatus::Idle.symbol(), "💬");
        assert_eq!(PromptStatus::Completed.symbol(), "✅");
        assert_eq!(PromptStatus::Failed.symbol(), "❌");
    }

    // ── Prompt::new ──

    #[test]
    fn new_prompt_defaults() {
        let p = Prompt::new(1, "hello".to_string(), None, PromptMode::Interactive);
        assert_eq!(p.id, 1);
        assert_eq!(p.text, "hello");
        assert_eq!(p.cwd, None);
        assert_eq!(p.mode, PromptMode::Interactive);
        assert_eq!(p.status, PromptStatus::Pending);
        assert!(p.output.is_none());
        assert!(p.error.is_none());
        assert!(p.started_at.is_none());
        assert!(p.finished_at.is_none());
        assert!(!p.seen);
    }

    #[test]
    fn new_prompt_with_cwd() {
        let p = Prompt::new(5, "test".to_string(), Some("/tmp".to_string()), PromptMode::OneShot);
        assert_eq!(p.cwd, Some("/tmp".to_string()));
        assert_eq!(p.mode, PromptMode::OneShot);
    }

    #[test]
    fn elapsed_secs_none_when_not_started() {
        let p = Prompt::new(1, "test".to_string(), None, PromptMode::Interactive);
        assert!(p.elapsed_secs().is_none());
    }

    // ── format_duration ──

    #[test]
    fn format_duration_under_60s() {
        assert_eq!(format_duration(0.0), "0.0s");
        assert_eq!(format_duration(4.23), "4.2s");
        assert_eq!(format_duration(59.9), "59.9s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60.0), "1m 0s");
        assert_eq!(format_duration(150.0), "2m 30s");
        assert_eq!(format_duration(3599.0), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600.0), "1h 0m");
        assert_eq!(format_duration(3900.0), "1h 5m");
        assert_eq!(format_duration(7261.0), "2h 1m");
    }

}
