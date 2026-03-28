pub mod adapter;

pub struct UserMessage {
    pub text: String,
    pub reply_tx: tokio::sync::oneshot::Sender<AgentResponse>,
}

pub struct AgentResponse {
    pub text: String,
}
