use crate::provider::moonshot::Message;
use serde::{Deserialize, Serialize};

/// A message with its lineage in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<usize>,
    pub message: Message,
}

/// Tracked message history with parent-linked entries.
pub struct EntryHistory {
    entries: Vec<Entry>,
    next_id: usize,
}

impl EntryHistory {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 0,
        }
    }

    /// Restore from persisted entries.
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        let next_id = entries.iter().map(|e| e.id + 1).max().unwrap_or(0);
        Self { entries, next_id }
    }

    /// Migrate legacy Vec<Message> — auto-assign sequential IDs, infer parents.
    pub fn from_legacy_messages(messages: Vec<Message>) -> Self {
        let entries: Vec<Entry> = messages
            .into_iter()
            .enumerate()
            .map(|(i, message)| {
                let parent_id = match &message {
                    Message::System { .. } | Message::User { .. } => None,
                    Message::Assistant { .. } | Message::Tool { .. } => {
                        if i > 0 {
                            Some(i - 1)
                        } else {
                            None
                        }
                    }
                };
                Entry {
                    id: i,
                    parent_id,
                    message,
                }
            })
            .collect();
        let next_id = entries.len();
        Self { entries, next_id }
    }

    fn push(&mut self, parent_id: Option<usize>, message: Message) -> usize {
        let id = self.next_id;
        self.entries.push(Entry {
            id,
            parent_id,
            message,
        });
        self.next_id += 1;
        id
    }

    /// Push a system message (no parent).
    pub fn push_system(&mut self, message: Message) -> usize {
        self.push(None, message)
    }

    /// Push a user message (root of a conversation turn, no parent).
    pub fn push_user(&mut self, message: Message) -> usize {
        self.push(None, message)
    }

    /// Push an assistant message (parent = immediate predecessor).
    pub fn push_assistant(&mut self, parent_id: usize, message: Message) -> usize {
        self.push(Some(parent_id), message)
    }

    /// Push a tool result (parent = assistant that requested it).
    pub fn push_tool(&mut self, parent_id: usize, message: Message) -> usize {
        self.push(Some(parent_id), message)
    }

    /// Project to API messages (strips tracking).
    pub fn messages(&self) -> Vec<Message> {
        self.entries.iter().map(|e| e.message.clone()).collect()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get entry by ID. Since IDs are sequential and entries are not removed
    /// (except eviction from the front), we search by ID.
    pub fn get(&self, id: usize) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Get mutable entry by ID.
    pub fn get_mut(&mut self, id: usize) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Updates the system message (Entry[0]). Used for dynamic prompt updates
    /// like injecting available reactions from the channel.
    pub fn update_system(&mut self, content: String) {
        if let Some(entry) = self.entries.first_mut() {
            if matches!(&entry.message, Message::System { .. }) {
                entry.message = Message::System { content };
            }
        }
    }

    /// ID of the last entry.
    pub fn last_id(&self) -> Option<usize> {
        self.entries.last().map(|e| e.id)
    }

    /// Walk parent chain to find the root entry (parent_id == None).
    pub fn root_entry_id(&self, entry_id: usize) -> Option<usize> {
        let entry = self.get(entry_id)?;
        match entry.parent_id {
            None => Some(entry.id),
            Some(pid) => self.root_entry_id(pid),
        }
    }

    /// Remove the two oldest entries (for context window eviction).
    pub fn evict_oldest_pair(&mut self) -> bool {
        if self.entries.len() >= 2 {
            self.entries.remove(0);
            self.entries.remove(0);
            true
        } else {
            false
        }
    }

    /// Remove entries by their IDs. Used by compaction strategies.
    pub fn remove_entries(&mut self, ids: &[usize]) {
        self.entries.retain(|e| !ids.contains(&e.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::moonshot::UserContent;

    #[test]
    fn test_entry_serialization_roundtrip() {
        let entry = Entry {
            id: 5,
            parent_id: Some(3),
            message: Message::User {
                content: UserContent::Text("hello".into()),
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, 5);
        assert_eq!(restored.parent_id, Some(3));
        assert_eq!(restored.message.content_text(), "hello");

        // None parent_id case
        let entry_no_parent = Entry {
            id: 0,
            parent_id: None,
            message: Message::System {
                content: "sys".into(),
            },
        };
        let json = serde_json::to_string(&entry_no_parent).unwrap();
        assert!(!json.contains("parent_id")); // skip_serializing_if
        let restored: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.parent_id, None);
    }

    #[test]
    fn test_from_legacy_messages() {
        let messages = vec![
            Message::System {
                content: "sys".into(),
            },
            Message::User {
                content: UserContent::Text("hi".into()),
            },
            Message::Assistant {
                content: Some("hello".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
            Message::Tool {
                tool_call_id: "t1".into(),
                name: None,
                content: "result".into(),
            },
        ];
        let history = EntryHistory::from_legacy_messages(messages);
        assert_eq!(history.len(), 4);

        // System: no parent
        assert_eq!(history.entries()[0].id, 0);
        assert_eq!(history.entries()[0].parent_id, None);

        // User: no parent
        assert_eq!(history.entries()[1].id, 1);
        assert_eq!(history.entries()[1].parent_id, None);

        // Assistant: parent = preceding (1)
        assert_eq!(history.entries()[2].id, 2);
        assert_eq!(history.entries()[2].parent_id, Some(1));

        // Tool: parent = preceding (2)
        assert_eq!(history.entries()[3].id, 3);
        assert_eq!(history.entries()[3].parent_id, Some(2));
    }

    #[test]
    fn test_push_methods_assign_ids() {
        let mut h = EntryHistory::new();
        let id0 = h.push_system(Message::System {
            content: "sys".into(),
        });
        let id1 = h.push_user(Message::User {
            content: UserContent::Text("hi".into()),
        });
        let id2 = h.push_assistant(
            id1,
            Message::Assistant {
                content: Some("hello".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        let id3 = h.push_tool(
            id2,
            Message::Tool {
                tool_call_id: "t1".into(),
                name: None,
                content: "result".into(),
            },
        );

        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);

        assert_eq!(h.get(id2).unwrap().parent_id, Some(id1));
        assert_eq!(h.get(id3).unwrap().parent_id, Some(id2));
    }

    #[test]
    fn test_messages_strips_tracking() {
        let mut h = EntryHistory::new();
        h.push_system(Message::System {
            content: "sys".into(),
        });
        h.push_user(Message::User {
            content: UserContent::Text("hi".into()),
        });

        let msgs = h.messages();
        assert_eq!(msgs.len(), 2);
        // Messages are plain Message enum — no id or parent_id
        let json = serde_json::to_string(&msgs[0]).unwrap();
        assert!(!json.contains("\"id\""));
        assert!(!json.contains("parent_id"));
    }

    #[test]
    fn test_root_entry_id_walks_chain() {
        let mut h = EntryHistory::new();
        let sys = h.push_system(Message::System {
            content: "sys".into(),
        });
        let user = h.push_user(Message::User {
            content: UserContent::Text("hi".into()),
        });
        let asst = h.push_assistant(
            user,
            Message::Assistant {
                content: Some("ok".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        let tool = h.push_tool(
            asst,
            Message::Tool {
                tool_call_id: "t".into(),
                name: None,
                content: "r".into(),
            },
        );

        // Root of user is itself
        assert_eq!(h.root_entry_id(user), Some(user));
        // Root of tool walks up: tool→asst→user
        assert_eq!(h.root_entry_id(tool), Some(user));
        // Root of system is itself
        assert_eq!(h.root_entry_id(sys), Some(sys));
    }

    #[test]
    fn test_update_system_replaces_entry_zero() {
        let mut h = EntryHistory::new();
        h.push_system(Message::System {
            content: "original".into(),
        });
        h.push_user(Message::User {
            content: UserContent::Text("hi".into()),
        });

        h.update_system("updated system prompt".into());

        assert_eq!(
            h.entries()[0].message.content_text(),
            "updated system prompt"
        );
        assert_eq!(h.messages()[0].content_text(), "updated system prompt");
        // User message unchanged
        assert_eq!(h.entries()[1].message.content_text(), "hi");
    }

    #[test]
    fn test_system_message_is_entry_zero() {
        let mut h = EntryHistory::new();
        h.push_system(Message::System {
            content: "test prompt".into(),
        });
        h.push_user(Message::User {
            content: UserContent::Text("hi".into()),
        });

        assert_eq!(h.entries()[0].id, 0);
        assert_eq!(h.entries()[0].parent_id, None);
        assert!(matches!(&h.entries()[0].message, Message::System { .. }));
    }
}
