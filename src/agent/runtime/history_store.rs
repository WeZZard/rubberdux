use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::agent::entry::{Entry, EntryHistory};
use crate::error::Error;
use crate::provider::moonshot::Message;

// ---------------------------------------------------------------------------
// HistoryStore trait — async persistence abstraction
// ---------------------------------------------------------------------------

pub trait HistoryStore: Send + Sync {
    fn persist<'a>(
        &'a self,
        entry: &'a Entry,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'a>>;

    fn load<'a>(
        &'a self,
        system_prompt: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<EntryHistory, Error>> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// FilesystemStore — persists to JSONL file
// ---------------------------------------------------------------------------

pub struct FilesystemStore {
    path: PathBuf,
}

impl FilesystemStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl HistoryStore for FilesystemStore {
    fn persist<'a>(
        &'a self,
        entry: &'a Entry,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(Error::Io)?;
            }

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
                .map_err(Error::Io)?;

            let json = serde_json::to_string(entry).map_err(Error::Json)?;
            tokio::io::AsyncWriteExt::write_all(&mut file, json.as_bytes())
                .await
                .map_err(Error::Io)?;
            tokio::io::AsyncWriteExt::write_all(&mut file, b"\n")
                .await
                .map_err(Error::Io)?;

            Ok(())
        })
    }

    fn load<'a>(
        &'a self,
        system_prompt: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<EntryHistory, Error>> + Send + 'a>> {
        Box::pin(async move {
            let file = match tokio::fs::File::open(&self.path).await {
                Ok(f) => f,
                Err(_) => {
                    let mut history = EntryHistory::new();
                    history.push_system(Message::System {
                        content: system_prompt.to_owned(),
                    });
                    return Ok(history);
                }
            };

            let reader = tokio::io::BufReader::new(file);
            let mut lines = tokio::io::AsyncBufReadExt::lines(reader);

            let mut entries: Vec<Entry> = Vec::new();
            let mut legacy_messages: Vec<Message> = Vec::new();
            let mut is_entry_format = false;
            let mut is_legacy_format = false;
            let mut line_num = 0usize;

            while let Ok(Some(line)) = lines.next_line().await {
                line_num += 1;

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

                log::warn!("Session line {} parse error", line_num);
            }

            let mut history = if is_entry_format {
                log::info!(
                    "Restored {} entries from session {:?}",
                    entries.len(),
                    self.path
                );
                EntryHistory::from_entries(entries)
            } else if is_legacy_format {
                log::info!(
                    "Restored {} legacy messages from session {:?}",
                    legacy_messages.len(),
                    self.path
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
                            let parent =
                                entry.parent_id.unwrap_or(new_history.last_id().unwrap_or(0));
                            new_history.push_assistant(parent, entry.message.clone());
                        }
                        Message::Tool { .. } => {
                            let parent =
                                entry.parent_id.unwrap_or(new_history.last_id().unwrap_or(0));
                            new_history.push_tool(parent, entry.message.clone());
                        }
                    }
                }
                history = new_history;
            }

            Ok(history)
        })
    }
}

// ---------------------------------------------------------------------------
// MemoryStore — in-memory store for testing
// ---------------------------------------------------------------------------

pub struct MemoryStore {
    entries: std::sync::Mutex<Vec<Entry>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl HistoryStore for MemoryStore {
    fn persist<'a>(
        &'a self,
        entry: &'a Entry,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut entries = self.entries.lock().unwrap();
            entries.push(entry.clone());
            Ok(())
        })
    }

    fn load<'a>(
        &'a self,
        system_prompt: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<EntryHistory, Error>> + Send + 'a>> {
        Box::pin(async move {
            let entries = self.entries.lock().unwrap();
            if entries.is_empty() {
                let mut history = EntryHistory::new();
                history.push_system(Message::System {
                    content: system_prompt.to_owned(),
                });
                Ok(history)
            } else {
                Ok(EntryHistory::from_entries(entries.clone()))
            }
        })
    }
}
