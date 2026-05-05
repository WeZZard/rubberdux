use tokio::sync::{broadcast, mpsc};

use crate::agent::entry::{Entry, EntryOrigin};
use crate::provider::moonshot::Message;

/// An event injected into the agent loop from any input source.
pub enum LoopEvent {
    /// A message to process via LLM.
    UserMessage {
        message: Message,
        origin: EntryOrigin,
        channel_metadata: Option<serde_json::Value>,
    },
    /// Context update to inject into history without triggering LLM processing.
    ContextUpdate(Message),
    /// Internal history/prompt mutation.
    Internal(InternalMutation),
}

/// History mutations that don't trigger LLM processing.
pub enum InternalMutation {
    /// Mutate a specific entry in history (e.g. inject channel-specific message ID).
    UpdateEntryContent {
        entry_id: usize,
        mutator: Box<dyn FnOnce(&mut Entry) + Send>,
    },
    /// Replace the system prompt content.
    UpdateSystemPrompt { content: String },
}

/// A handle for sending events into an AgentLoop. Cloneable.
#[derive(Clone)]
pub struct InputPort {
    tx: mpsc::Sender<LoopEvent>,
}

impl InputPort {
    pub fn new(tx: mpsc::Sender<LoopEvent>) -> Self {
        Self { tx }
    }

    pub async fn send(&self, event: LoopEvent) -> Result<(), crate::error::Error> {
        self.tx
            .send(event)
            .await
            .map_err(|_| crate::error::Error::ChannelClosed)
    }

    pub async fn send_user_message(
        &self,
        message: Message,
        origin: EntryOrigin,
    ) -> Result<(), crate::error::Error> {
        self.send(LoopEvent::UserMessage {
            message,
            origin,
            channel_metadata: None,
        })
        .await
    }

    pub async fn send_user_message_with_metadata(
        &self,
        message: Message,
        origin: EntryOrigin,
        channel_metadata: Option<serde_json::Value>,
    ) -> Result<(), crate::error::Error> {
        self.send(LoopEvent::UserMessage {
            message,
            origin,
            channel_metadata,
        })
        .await
    }

    pub async fn send_context_update(&self, message: Message) -> Result<(), crate::error::Error> {
        self.send(LoopEvent::ContextUpdate(message)).await
    }
}

/// Notification broadcast after an entry is added to history.
#[derive(Debug, Clone)]
pub struct EntryNotification {
    pub entry: Entry,
    pub is_final: bool,
}

/// A handle for observing history entries from an AgentLoop.
pub struct OutputPort {
    rx: broadcast::Receiver<EntryNotification>,
}

impl OutputPort {
    pub fn new(rx: broadcast::Receiver<EntryNotification>) -> Self {
        Self { rx }
    }

    pub fn into_receiver(self) -> broadcast::Receiver<EntryNotification> {
        self.rx
    }

    pub async fn recv(&mut self) -> Option<EntryNotification> {
        self.rx.recv().await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::entry::EntryOrigin;
    use crate::provider::moonshot::{Message, UserContent};

    #[tokio::test]
    async fn test_input_port_send_user_message_with_origin() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let port = InputPort::new(tx);

        port.send_user_message(
            Message::User { content: UserContent::Text("hello".into()) },
            EntryOrigin::User { channel: "test".into() },
        ).await.unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            LoopEvent::UserMessage { origin, .. } => {
                assert_eq!(origin, EntryOrigin::User { channel: "test".into() });
            }
            _ => panic!("Expected UserMessage"),
        }
    }
}
