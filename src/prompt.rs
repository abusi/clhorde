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

#[derive(Debug)]
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
        }
    }
}
