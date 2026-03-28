use std::path::{Path, PathBuf};

const DEFAULT_PROMPT_DIR: &str = "./prompts";

pub fn prompt_dir() -> PathBuf {
    std::env::var("RUBBERDUX_PROMPT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PROMPT_DIR))
}

pub fn load_system_prompt(prompt_dir: &Path) -> Result<String, std::io::Error> {
    let path = prompt_dir.join("system.txt");
    std::fs::read_to_string(&path)
}
