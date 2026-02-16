use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Saved elapsed seconds (used when restoring from session).
    pub saved_elapsed: Option<f64>,
    /// Live PTY terminal state (only for running interactive/PTY workers).
    pub pty_state: Option<crate::pty_worker::SharedPtyState>,
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
            saved_elapsed: None,
            pty_state: None,
        }
    }

    pub fn elapsed_secs(&self) -> Option<f64> {
        if let Some(saved) = self.saved_elapsed {
            return Some(saved);
        }
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Instant::now);
        Some(end.duration_since(start).as_secs_f64())
    }
}

/// Serializable version of Prompt for session persistence.
#[derive(Serialize, Deserialize)]
pub struct SerializablePrompt {
    pub id: usize,
    pub text: String,
    pub cwd: Option<String>,
    pub mode: PromptMode,
    pub status: PromptStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub elapsed_secs: Option<f64>,
}

impl From<&Prompt> for SerializablePrompt {
    fn from(p: &Prompt) -> Self {
        Self {
            id: p.id,
            text: p.text.clone(),
            cwd: p.cwd.clone(),
            mode: p.mode,
            status: p.status.clone(),
            output: p.output.clone(),
            error: p.error.clone(),
            elapsed_secs: p.elapsed_secs(),
        }
    }
}

impl SerializablePrompt {
    pub fn into_prompt(self) -> Prompt {
        // Running/Idle prompts become Completed on restore (process is gone)
        let status = match self.status {
            PromptStatus::Running | PromptStatus::Idle => PromptStatus::Completed,
            other => other,
        };
        Prompt {
            id: self.id,
            text: self.text,
            cwd: self.cwd,
            mode: self.mode,
            status,
            output: self.output,
            error: self.error,
            started_at: None,
            finished_at: None,
            seen: true, // restored prompts are considered seen
            saved_elapsed: self.elapsed_secs,
            pty_state: None,
        }
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
        assert!(p.saved_elapsed.is_none());
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

    #[test]
    fn elapsed_secs_uses_saved_value() {
        let mut p = Prompt::new(1, "test".to_string(), None, PromptMode::Interactive);
        p.saved_elapsed = Some(42.5);
        assert_eq!(p.elapsed_secs(), Some(42.5));
    }

    // ── SerializablePrompt::into_prompt ──

    fn make_serializable(status: PromptStatus) -> SerializablePrompt {
        SerializablePrompt {
            id: 1,
            text: "test".to_string(),
            cwd: Some("/home".to_string()),
            mode: PromptMode::Interactive,
            status,
            output: Some("output".to_string()),
            error: None,
            elapsed_secs: Some(5.0),
        }
    }

    #[test]
    fn into_prompt_running_becomes_completed() {
        let sp = make_serializable(PromptStatus::Running);
        let p = sp.into_prompt();
        assert_eq!(p.status, PromptStatus::Completed);
    }

    #[test]
    fn into_prompt_idle_becomes_completed() {
        let sp = make_serializable(PromptStatus::Idle);
        let p = sp.into_prompt();
        assert_eq!(p.status, PromptStatus::Completed);
    }

    #[test]
    fn into_prompt_pending_stays_pending() {
        let sp = make_serializable(PromptStatus::Pending);
        let p = sp.into_prompt();
        assert_eq!(p.status, PromptStatus::Pending);
    }

    #[test]
    fn into_prompt_completed_stays_completed() {
        let sp = make_serializable(PromptStatus::Completed);
        let p = sp.into_prompt();
        assert_eq!(p.status, PromptStatus::Completed);
    }

    #[test]
    fn into_prompt_failed_stays_failed() {
        let sp = make_serializable(PromptStatus::Failed);
        let p = sp.into_prompt();
        assert_eq!(p.status, PromptStatus::Failed);
    }

    #[test]
    fn into_prompt_fields_preserved() {
        let sp = make_serializable(PromptStatus::Completed);
        let p = sp.into_prompt();
        assert_eq!(p.id, 1);
        assert_eq!(p.text, "test");
        assert_eq!(p.cwd, Some("/home".to_string()));
        assert_eq!(p.mode, PromptMode::Interactive);
        assert_eq!(p.output, Some("output".to_string()));
        assert!(p.error.is_none());
        assert_eq!(p.saved_elapsed, Some(5.0));
        assert!(p.seen); // restored prompts are seen
        assert!(p.started_at.is_none());
        assert!(p.finished_at.is_none());
    }
}
