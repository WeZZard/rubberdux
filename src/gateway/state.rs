use tokio::sync::{RwLock, broadcast};

use crate::agent::entry::Entry;
use crate::agent::runtime::port::{EntryNotification, InputPort};
use crate::trajectory::TrajectoryEvent;

pub struct GatewayState {
    pub entries: RwLock<Vec<Entry>>,
    pub system_prompt: String,
    pub identity_prompt: String,
    pub soul_prompt: String,
    pub entry_tx: broadcast::Sender<EntryNotification>,
    pub trajectory_tx: broadcast::Sender<TrajectoryEvent>,
    pub input_port: InputPort,
    pub events_path: Option<std::path::PathBuf>,
}

impl GatewayState {
    pub fn new(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
        input_port: InputPort,
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
            input_port,
            events_path: None,
        }
    }

    pub fn with_trajectory_tx(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
        trajectory_tx: broadcast::Sender<TrajectoryEvent>,
        input_port: InputPort,
        events_path: Option<std::path::PathBuf>,
    ) -> Self {
        let (entry_tx, _) = broadcast::channel(256);
        Self {
            entries: RwLock::new(Vec::new()),
            system_prompt,
            identity_prompt,
            soul_prompt,
            entry_tx,
            trajectory_tx,
            input_port,
            events_path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::entry::EntryOrigin;
    use crate::agent::runtime::port::LoopEvent;
    use crate::provider::moonshot::{Message, UserContent};

    fn dummy_input_port() -> InputPort {
        let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
        InputPort::new(tx)
    }

    #[test]
    fn test_new_creates_empty_state() {
        let state = GatewayState::new(
            "system".into(),
            "identity".into(),
            "soul".into(),
            dummy_input_port(),
        );
        let entries = state.entries.blocking_read();
        assert!(entries.is_empty());
        assert_eq!(state.system_prompt, "system");
        assert_eq!(state.identity_prompt, "identity");
        assert_eq!(state.soul_prompt, "soul");
    }

    #[tokio::test]
    async fn test_push_entry_updates_state() {
        let state = GatewayState::new("sys".into(), "id".into(), "soul".into(), dummy_input_port());
        let entry = Entry {
            id: 0,
            parent_id: None,
            message: Message::User {
                content: UserContent::Text("hello".into()),
            },
            origin: EntryOrigin::User { channel: "test".into() },
            channel_metadata: None,
        };
        state.entries.write().await.push(entry);

        let entries = state.entries.read().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 0);
        assert_eq!(entries[0].message.content_text(), "hello");
    }
}
