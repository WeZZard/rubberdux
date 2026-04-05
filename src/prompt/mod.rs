use std::path::{Path, PathBuf};

const DEFAULT_PROMPT_DIR: &str = "./prompts";

pub fn prompt_dir() -> PathBuf {
    std::env::var("RUBBERDUX_PROMPT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PROMPT_DIR))
}

/// Loads prompt parts from the prompt directory in order: IDENTITY.md, SOUL.md.
pub fn load_prompt_parts(prompt_dir: &Path) -> Vec<String> {
    let mut parts = Vec::new();
    for name in ["IDENTITY.md", "SOUL.md"] {
        let path = prompt_dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                log::info!("Loaded prompt part: {:?}", path);
                parts.push(content);
            }
            Err(e) => {
                log::warn!("Failed to load prompt part {:?}: {}", path, e);
            }
        }
    }
    parts
}

/// Built-in guardrails that cannot be modified by end users.
const GUARDRAILS: &str = include_str!("GUARDRAILS.md");

/// Composes a system prompt from parts and an optional channel partial.
pub fn compose_system_prompt(parts: &[String], channel_partial: Option<&str>) -> String {
    let mut prompt = parts.join("\n\n");
    prompt.push_str("\n\n");
    prompt.push_str(GUARDRAILS);
    if let Some(partial) = channel_partial {
        prompt.push_str("\n\n");
        prompt.push_str(partial);
    }
    prompt
}
