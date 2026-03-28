pub mod adapter;
pub mod interpreter;

use interpreter::InterpretedMessage;

pub struct UserMessage {
    pub interpreted: InterpretedMessage,
    pub reply_tx: Option<tokio::sync::oneshot::Sender<AgentResponse>>,
}

pub struct AgentResponse {
    pub text: String,
    pub history_index: usize,
}
