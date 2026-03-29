pub mod adapter;
pub mod interpreter;

use interpreter::InterpretedMessage;

/// An event from a channel to the agent loop.
pub enum ChannelEvent {
    /// User input that should be processed by the LLM.
    UserInput {
        interpreted: InterpretedMessage,
        reply_tx: Option<tokio::sync::mpsc::Sender<AgentResponse>>,
    },
    /// Channel-internal event that mutates history without calling the LLM.
    InternalEvent(InternalEvent),
}

/// Channel-specific internal events.
pub enum InternalEvent {
    /// Associates a channel-side message ID with an assistant message in history.
    UpdateAssistantMessageId {
        history_index: usize,
        message_id: i32,
    },
}

pub struct AgentResponse {
    pub text: String,
    pub history_index: usize,
    pub is_final: bool,
}
