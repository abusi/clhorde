use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
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
    pub status: PromptStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    /// Whether the user has seen/acknowledged this prompt's completion.
    pub seen: bool,
}

impl Prompt {
    pub fn new(id: usize, text: String, cwd: Option<String>) -> Self {
        Self {
            id,
            text,
            cwd,
            status: PromptStatus::Pending,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            seen: false,
        }
    }

    pub fn elapsed_secs(&self) -> Option<f64> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Instant::now);
        Some(end.duration_since(start).as_secs_f64())
    }
}
