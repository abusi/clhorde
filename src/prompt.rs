use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub enum PromptStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl PromptStatus {
    pub fn symbol(&self) -> &str {
        match self {
            PromptStatus::Pending => "⏳",
            PromptStatus::Running => "🔄",
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
        }
    }

    pub fn elapsed_secs(&self) -> Option<f64> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Instant::now);
        Some(end.duration_since(start).as_secs_f64())
    }
}
