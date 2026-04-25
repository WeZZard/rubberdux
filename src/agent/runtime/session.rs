use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::agent::entry::{Entry, EntryHistory};
use crate::provider::moonshot::Message;

const DEFAULT_SESSION_DIR: &str = "./sessions";
const SESSION_FILENAME: &str = "session.jsonl";

pub fn session_path() -> PathBuf {
    let dir = std::env::var("RUBBERDUX_SESSION_DIR").unwrap_or_else(|_| DEFAULT_SESSION_DIR.into());
    PathBuf::from(dir).join(SESSION_FILENAME)
}

pub fn load_session(path: &Path, system_prompt: &str) -> EntryHistory {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            let mut history = EntryHistory::new();
            history.push_system(Message::System {
                content: system_prompt.to_owned(),
            });
            return history;
        }
    };

    let reader = std::io::BufReader::new(file);
    let mut entries: Vec<Entry> = Vec::new();
    let mut legacy_messages: Vec<Message> = Vec::new();
    let mut is_entry_format = false;
    let mut is_legacy_format = false;

    for (i, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log::warn!("Session line {} read error: {}", i, e);
                continue;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<Entry>(&line) {
            entries.push(entry);
            is_entry_format = true;
            continue;
        }

        if let Ok(msg) = serde_json::from_str::<Message>(&line) {
            legacy_messages.push(msg);
            is_legacy_format = true;
            continue;
        }

        log::warn!("Session line {} parse error", i);
    }

    let mut history = if is_entry_format {
        log::info!("Restored {} entries from session {:?}", entries.len(), path);
        EntryHistory::from_entries(entries)
    } else if is_legacy_format {
        log::info!(
            "Restored {} legacy messages from session {:?}",
            legacy_messages.len(),
            path
        );
        EntryHistory::from_legacy_messages(legacy_messages)
    } else {
        EntryHistory::new()
    };

    // Ensure system message is Entry[0]
    if history.is_empty()
        || !matches!(
            history.entries().first().map(|e| &e.message),
            Some(Message::System { .. })
        )
    {
        let mut new_history = EntryHistory::new();
        new_history.push_system(Message::System {
            content: system_prompt.to_owned(),
        });
        for entry in history.entries() {
            match &entry.message {
                Message::System { .. } => {}
                Message::User { .. } => {
                    new_history.push_user(entry.message.clone());
                }
                Message::Assistant { .. } => {
                    let parent = entry
                        .parent_id
                        .unwrap_or(new_history.last_id().unwrap_or(0));
                    new_history.push_assistant(parent, entry.message.clone());
                }
                Message::Tool { .. } => {
                    let parent = entry
                        .parent_id
                        .unwrap_or(new_history.last_id().unwrap_or(0));
                    new_history.push_tool(parent, entry.message.clone());
                }
            }
        }
        history = new_history;
    }

    history
}

pub fn append_entry_to_session(path: &Path, entry: &Entry) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open session file {:?}: {}", path, e);
            return;
        }
    };

    match serde_json::to_string(entry) {
        Ok(json) => {
            if writeln!(file, "{}", json).is_err() {
                log::error!("Failed to write to session file");
            }
        }
        Err(e) => log::error!("Failed to serialize entry: {}", e),
    }
}
