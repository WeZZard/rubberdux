use std::future::Future;
use std::pin::Pin;

/// Error from LLM operations.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Request failed: {0}")]
    Request(String),
    #[error("Response parsing failed: {0}")]
    Parse(String),
}

/// Abstract LLM client for test operations.
///
/// Implementors provide the HTTP transport. `md-testing` handles JSON
/// serialization of the OpenAI-compatible chat format and deserialization.
pub trait LlmClient: Send + Sync {
    /// Send a chat completion request.
    ///
    /// `body` is the JSON-serialized request body (OpenAI chat.completions format).
    /// Implementors POST this to their endpoint and return the raw response body.
    fn chat_raw(
        &self,
        body: String,
    ) -> Pin<Box<dyn Future<Output = Result<String, LlmError>> + Send + '_>>;
}

/// A minimal message type for LLM chat requests (evaluator, guidance).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Build an OpenAI-compatible chat request JSON string.
pub fn build_request_json(model: &str, messages: &[ChatMessage], temperature: f64) -> String {
    serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
    })
    .to_string()
}

/// Parse an OpenAI-compatible chat response and extract assistant text.
pub fn parse_response_text(raw: &str) -> Result<String, LlmError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| LlmError::Parse(e.to_string()))?;

    let text = value["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| LlmError::Parse("Missing content in response".into()))?;

    Ok(text.to_string())
}

/// Sanitize a string for safe JSON parsing.
/// Removes control characters that break JSON parsers, and normalizes
/// newlines inside JSON string values to spaces so the LLM can emit
/// multi-line reasoning without producing invalid JSON.
pub fn sanitize_for_json(text: &str) -> String {
    let mut result = text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t')
        .collect::<String>();

    // Replace newlines with spaces to keep JSON string values single-line
    result = result.replace('\n', " ").replace('\r', " ");

    // Collapse multiple spaces
    while result.contains("  ") {
        result = result.replace("  ", " ");
    }

    result
}
