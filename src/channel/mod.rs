pub mod adapter;
pub mod interpreter;

use interpreter::InterpretedMessage;

/// An event from a channel to the agent loop.
pub enum ChannelEvent {
    /// User input that should be processed by the LLM.
    UserInput {
        interpreted: InterpretedMessage,
        reply_tx: Option<tokio::sync::mpsc::Sender<AgentResponse>>,
        telegram_message_id: Option<i32>,
    },
    /// Channel-internal event that mutates history without calling the LLM.
    InternalEvent(InternalEvent),
}

/// Channel-specific internal events.
pub enum InternalEvent {
    /// Associates a channel-side message ID with an assistant message in history.
    UpdateAssistantMessageId {
        entry_id: usize,
        message_id: i32,
    },
    /// Provides an updated reaction section for the system prompt.
    UpdateAvailableReactions {
        reaction_section: String,
    },
}

pub struct AgentResponse {
    pub text: String,
    pub entry_id: usize,
    pub is_final: bool,
    pub reply_to_message_id: Option<i32>,
}
