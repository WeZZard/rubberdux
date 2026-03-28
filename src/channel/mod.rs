pub mod adapter;
pub mod interpreter;

use interpreter::InterpretedMessage;

pub struct UserMessage {
    pub interpreted: InterpretedMessage,
    pub reply_tx: tokio::sync::oneshot::Sender<AgentResponse>,
}

pub struct AgentResponse {
    pub text: String,
}
