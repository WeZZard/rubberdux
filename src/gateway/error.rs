use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("entry not found: {0}")]
    EntryNotFound(usize),

    #[error("app not found: {0}")]
    AppNotFound(String),

    #[error("interaction not found: {0}")]
    InteractionNotFound(String),

    #[error("supervisor error: {0}")]
    Supervisor(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The selected provider's API returned an error. Mapped to 502 so callers
    /// can distinguish a provider-side fault from a gateway-side fault.
    #[error("provider error: {0}")]
    ProviderError(String),
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = match &self {
            GatewayError::EntryNotFound(_)
            | GatewayError::AppNotFound(_)
            | GatewayError::InteractionNotFound(_) => StatusCode::NOT_FOUND,
            GatewayError::ProviderError(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}
