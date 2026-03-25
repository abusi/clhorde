//! Path helpers and file I/O for config, history, and templates.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Controls whether git worktrees are cleaned up automatically or manually.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WorktreeCleanup {
    Manual,
    Auto,
}

/// Base data directory: `~/.local/share/clhorde/`
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("clhorde"))
}

/// Path to prompt history file: `~/.local/share/clhorde/history`
pub fn history_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("history"))
}

/// Path to templates config: `~/.config/clhorde/templates.toml`
pub fn templates_path() -> Option<PathBuf> {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(config_dir.join("clhorde").join("templates.toml"))
}

/// Load prompt history from disk. Each line is one entry; `\n` within entries
/// is escaped as `\\n`.
pub fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    match fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.replace("\\n", "\n"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Append a single entry to the history file on disk.
pub fn append_history(text: &str) {
    if let Some(path) = history_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let escaped = text.replace('\n', "\\n");
            let _ = writeln!(file, "{escaped}");
        }
    }
}

/// Load prompt templates from `templates.toml`.
pub fn load_templates() -> HashMap<String, String> {
    let Some(path) = templates_path() else {
        return HashMap::new();
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    #[derive(serde::Deserialize)]
    struct TemplateConfig {
        templates: Option<HashMap<String, String>>,
    }

    match toml::from_str::<TemplateConfig>(&content) {
        Ok(config) => config.templates.unwrap_or_default(),
        Err(_) => {
            // Try as flat key-value pairs (no [templates] section)
            toml::from_str::<HashMap<String, String>>(&content).unwrap_or_default()
        }
    }
}
