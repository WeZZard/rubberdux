#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("telegram error: {0}")]
    Telegram(#[from] teloxide::RequestError),

    #[error("openai error: {0}")]
    OpenAI(#[from] async_openai::error::OpenAIError),

    #[error("prompt load error: {0}")]
    PromptLoad(#[from] std::io::Error),

    #[error("agent channel closed")]
    ChannelClosed,
}
