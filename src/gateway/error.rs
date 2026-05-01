use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("entry not found: {0}")]
    EntryNotFound(usize),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = match &self {
            GatewayError::EntryNotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}
