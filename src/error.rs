#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("telegram error: {0}")]
    Telegram(#[from] teloxide::RequestError),

    #[error("provider HTTP error: {0}")]
    ProviderHttp(#[from] reqwest::Error),

    #[error("provider API error (status {status}): {body}")]
    ProviderApi { status: u16, body: String },

    #[error("provider error: {0}")]
    Provider(String),

    #[error("prompt load error: {0}")]
    PromptLoad(#[from] std::io::Error),

    #[error("agent channel closed")]
    ChannelClosed,
}
