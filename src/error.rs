#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("telegram error: {0}")]
    Telegram(#[from] teloxide::RequestError),

    #[error("provider HTTP error: {0}")]
    ProviderHttp(#[from] reqwest::Error),

    #[error("provider API error (status {status}): {body}")]
    ProviderApi { status: u16, body: String },

    #[error("provider error: {0}")]
    Provider(String),

    #[error("subagent error: {0}")]
    Subagent(String),

    #[error("agent channel closed")]
    ChannelClosed,

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("VM error: {0}")]
    Vm(String),
}
