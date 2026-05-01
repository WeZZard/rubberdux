use tokio::sync::{RwLock, broadcast};

use crate::agent::entry::Entry;
use crate::agent::runtime::port::EntryNotification;
use crate::trajectory::TrajectoryEvent;

pub struct GatewayState {
    pub entries: RwLock<Vec<Entry>>,
    pub system_prompt: String,
    pub identity_prompt: String,
    pub soul_prompt: String,
    pub entry_tx: broadcast::Sender<EntryNotification>,
    pub trajectory_tx: broadcast::Sender<TrajectoryEvent>,
}

impl GatewayState {
    pub fn new(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
    ) -> Self {
        let (entry_tx, _) = broadcast::channel(256);
        let (trajectory_tx, _) = broadcast::channel(256);
        Self {
            entries: RwLock::new(Vec::new()),
            system_prompt,
            identity_prompt,
            soul_prompt,
            entry_tx,
            trajectory_tx,
        }
    }

    pub fn with_trajectory_tx(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
        trajectory_tx: broadcast::Sender<TrajectoryEvent>,
    ) -> Self {
        let (entry_tx, _) = broadcast::channel(256);
        Self {
            entries: RwLock::new(Vec::new()),
            system_prompt,
            identity_prompt,
            soul_prompt,
            entry_tx,
            trajectory_tx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::moonshot::{Message, UserContent};

    #[test]
    fn test_new_creates_empty_state() {
        let state = GatewayState::new(
            "system".into(),
            "identity".into(),
            "soul".into(),
        );
        let entries = state.entries.blocking_read();
        assert!(entries.is_empty());
        assert_eq!(state.system_prompt, "system");
        assert_eq!(state.identity_prompt, "identity");
        assert_eq!(state.soul_prompt, "soul");
    }

    #[tokio::test]
    async fn test_push_entry_updates_state() {
        let state = GatewayState::new("sys".into(), "id".into(), "soul".into());
        let entry = Entry {
            id: 0,
            parent_id: None,
            message: Message::User {
                content: UserContent::Text("hello".into()),
            },
        };
        state.entries.write().await.push(entry);

        let entries = state.entries.read().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 0);
        assert_eq!(entries[0].message.content_text(), "hello");
    }
}
