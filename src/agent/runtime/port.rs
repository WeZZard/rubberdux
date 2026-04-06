use tokio::sync::{broadcast, mpsc};

use crate::agent::entry::Entry;
use crate::provider::moonshot::Message;

/// An event injected into the agent loop from any input source.
pub enum LoopEvent {
    /// A message to process via LLM.
    UserMessage {
        message: Message,
        /// Where to send the response. None = silent injection (no response expected).
        reply: Option<mpsc::Sender<LoopOutput>>,
        /// Opaque metadata from the input source (e.g. telegram_message_id).
        metadata: Option<Box<dyn std::any::Any + Send>>,
    },
    /// Context update to inject into history without triggering LLM processing.
    ContextUpdate(Message),
    /// Internal history/prompt mutation.
    Internal(InternalMutation),
}

/// An output emitted by the agent loop for a conversation response.
pub struct LoopOutput {
    pub text: String,
    pub entry_id: usize,
    pub is_final: bool,
    /// Opaque metadata forwarded from the input event.
    pub metadata: Option<Box<dyn std::any::Any + Send>>,
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
        reply: Option<mpsc::Sender<LoopOutput>>,
    ) -> Result<(), crate::error::Error> {
        self.send(LoopEvent::UserMessage {
            message,
            reply,
            metadata: None,
        })
        .await
    }

    pub async fn send_context_update(
        &self,
        message: Message,
    ) -> Result<(), crate::error::Error> {
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

    pub async fn recv(&mut self) -> Option<EntryNotification> {
        self.rx.recv().await.ok()
    }
}
