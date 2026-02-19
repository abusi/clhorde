use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::prompt::PromptMode;

#[derive(Serialize, Deserialize)]
pub struct PromptFile {
    pub prompt: String,
    pub options: PromptOptions,
    pub state: String,
    pub queue_rank: f64,
    pub session_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PromptOptions {
    pub mode: String,
    pub context: Option<String>,
}

pub fn default_prompts_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("clhorde").join("prompts"))
}

pub fn save_prompt(dir: &Path, uuid: &str, data: &PromptFile) {
    let _ = fs::create_dir_all(dir);
    let path = dir.join(format!("{uuid}.json"));
    let content = match serde_json::to_string_pretty(data) {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = fs::write(path, content);
}

pub fn load_all_prompts(dir: &Path) -> Vec<(String, PromptFile)> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut prompts: Vec<(String, PromptFile)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let uuid = path.file_stem()?.to_str()?.to_string();
            let content = fs::read_to_string(&path).ok()?;
            let data: PromptFile = serde_json::from_str(&content).ok()?;
            Some((uuid, data))
        })
        .collect();

    prompts.sort_by(|a, b| {
        a.1.queue_rank
            .partial_cmp(&b.1.queue_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    prompts
}

pub fn prune_old_prompts(dir: &Path, max: usize) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            path.file_stem()?.to_str().map(String::from)
        })
        .collect();

    if files.len() <= max {
        return;
    }

    // UUID v7 sorts lexicographically by time — sort ascending
    files.sort();

    // Delete the oldest (first) entries, keep the last `max`
    let to_delete = files.len() - max;
    for uuid in &files[..to_delete] {
        let path = dir.join(format!("{uuid}.json"));
        let _ = fs::remove_file(path);
    }
}

pub fn delete_prompt_file(dir: &Path, uuid: &str) {
    let path = dir.join(format!("{uuid}.json"));
    let _ = fs::remove_file(path);
}

impl PromptFile {
    pub fn from_prompt(prompt: &crate::prompt::Prompt) -> Self {
        let mode = match prompt.mode {
            PromptMode::Interactive => "interactive",
            PromptMode::OneShot => "one_shot",
        };
        let state = match prompt.status {
            crate::prompt::PromptStatus::Pending => "pending",
            crate::prompt::PromptStatus::Running => "running",
            crate::prompt::PromptStatus::Idle => "running",
            crate::prompt::PromptStatus::Completed => "completed",
            crate::prompt::PromptStatus::Failed => "failed",
        };
        PromptFile {
            prompt: prompt.text.clone(),
            options: PromptOptions {
                mode: mode.to_string(),
                context: prompt.cwd.clone(),
            },
            state: state.to_string(),
            queue_rank: prompt.queue_rank,
            session_id: prompt.session_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_prompts_dir() -> PathBuf {
        let dir = env::temp_dir().join(format!("clhorde-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_prompts_dir();

        let uuid1 = uuid::Uuid::now_v7().to_string();
        let data = PromptFile {
            prompt: "test prompt".to_string(),
            options: PromptOptions {
                mode: "interactive".to_string(),
                context: Some("/tmp".to_string()),
            },
            state: "completed".to_string(),
            queue_rank: 1.0,
            session_id: Some("sess-123".to_string()),
        };

        save_prompt(&dir, &uuid1, &data);
        let loaded = load_all_prompts(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, uuid1);
        assert_eq!(loaded[0].1.prompt, "test prompt");
        assert_eq!(loaded[0].1.options.mode, "interactive");
        assert_eq!(loaded[0].1.options.context, Some("/tmp".to_string()));
        assert_eq!(loaded[0].1.state, "completed");
        assert_eq!(loaded[0].1.queue_rank, 1.0);
        assert_eq!(loaded[0].1.session_id, Some("sess-123".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_empty_dir() {
        let dir = temp_prompts_dir();
        let loaded = load_all_prompts(&dir);
        assert!(loaded.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_nonexistent_dir() {
        let dir = PathBuf::from("/tmp/clhorde-nonexistent-dir-xyz");
        let loaded = load_all_prompts(&dir);
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_sorts_by_queue_rank() {
        let dir = temp_prompts_dir();

        for (i, rank) in [(0, 3.0), (1, 1.0), (2, 2.0)] {
            let uuid = uuid::Uuid::now_v7().to_string();
            let data = PromptFile {
                prompt: format!("prompt {i}"),
                options: PromptOptions {
                    mode: "interactive".to_string(),
                    context: None,
                },
                state: "completed".to_string(),
                queue_rank: rank,
                session_id: None,
            };
            save_prompt(&dir, &uuid, &data);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let loaded = load_all_prompts(&dir);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].1.queue_rank, 1.0);
        assert_eq!(loaded[1].1.queue_rank, 2.0);
        assert_eq!(loaded[2].1.queue_rank, 3.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prompt_file_from_prompt() {
        let prompt = crate::prompt::Prompt::new(
            1,
            "hello".to_string(),
            Some("/tmp".to_string()),
            PromptMode::Interactive,
        );
        let file = PromptFile::from_prompt(&prompt);
        assert_eq!(file.prompt, "hello");
        assert_eq!(file.options.mode, "interactive");
        assert_eq!(file.options.context, Some("/tmp".to_string()));
        assert_eq!(file.state, "pending");
        assert!(file.session_id.is_none());
    }

    #[test]
    fn prompt_file_oneshot_mode() {
        let prompt = crate::prompt::Prompt::new(1, "test".to_string(), None, PromptMode::OneShot);
        let file = PromptFile::from_prompt(&prompt);
        assert_eq!(file.options.mode, "one_shot");
        assert!(file.options.context.is_none());
    }

    #[test]
    fn prune_keeps_newest() {
        let dir = temp_prompts_dir();

        let mut uuids = Vec::new();
        for i in 0..5 {
            let uuid = uuid::Uuid::now_v7().to_string();
            let data = PromptFile {
                prompt: format!("prompt {i}"),
                options: PromptOptions {
                    mode: "interactive".to_string(),
                    context: None,
                },
                state: "completed".to_string(),
                queue_rank: i as f64,
                session_id: None,
            };
            save_prompt(&dir, &uuid, &data);
            uuids.push(uuid);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(load_all_prompts(&dir).len(), 5);
        prune_old_prompts(&dir, 3);

        let remaining = load_all_prompts(&dir);
        assert_eq!(remaining.len(), 3);
        // Oldest two should be gone
        let remaining_uuids: Vec<&str> = remaining.iter().map(|(u, _)| u.as_str()).collect();
        assert!(!remaining_uuids.contains(&uuids[0].as_str()));
        assert!(!remaining_uuids.contains(&uuids[1].as_str()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_noop_when_under_limit() {
        let dir = temp_prompts_dir();

        let uuid = uuid::Uuid::now_v7().to_string();
        let data = PromptFile {
            prompt: "only one".to_string(),
            options: PromptOptions {
                mode: "interactive".to_string(),
                context: None,
            },
            state: "completed".to_string(),
            queue_rank: 1.0,
            session_id: None,
        };
        save_prompt(&dir, &uuid, &data);

        prune_old_prompts(&dir, 10);
        assert_eq!(load_all_prompts(&dir).len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_prompt() {
        let dir = temp_prompts_dir();

        let uuid = uuid::Uuid::now_v7().to_string();
        let data = PromptFile {
            prompt: "to delete".to_string(),
            options: PromptOptions {
                mode: "interactive".to_string(),
                context: None,
            },
            state: "completed".to_string(),
            queue_rank: 1.0,
            session_id: None,
        };
        save_prompt(&dir, &uuid, &data);
        assert_eq!(load_all_prompts(&dir).len(), 1);

        delete_prompt_file(&dir, &uuid);
        assert_eq!(load_all_prompts(&dir).len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
