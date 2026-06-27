//! model_client — see docs/agent/world/ecs-runtime.md

use serde::Deserialize;
use serde_json::Value as Json;

use super::history::Block;
use super::inputs::{Capabilities, ModelMeta, ReasoningPolicy, StopReason, Usage};
use crate::error::Error;

// ---------------------------------------------------------------------------
// MessagesClient — thin Anthropic /v1/messages client
// ---------------------------------------------------------------------------

/// A thin HTTP client for the Anthropic `POST /v1/messages` surface.
///
/// Reads configuration from environment variables on construction.
/// Distinct from the OpenAI-shape `MoonshotClient`; targets the Anthropic
/// Messages API (or a compatible endpoint whose base URL is configurable).
/// See docs/agent/world/ecs-runtime.md (Anthropic model-call mapping).
pub struct MessagesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl MessagesClient {
    pub fn new(http: reqwest::Client, base_url: String, api_key: String, model: String) -> Self {
        Self {
            http,
            base_url,
            api_key,
            model,
        }
    }

    /// Construct from environment variables.
    ///
    /// | Variable                 | Default                        |
    /// |--------------------------|--------------------------------|
    /// | `RUBBERDUX_LLM_API_KEY`  | `""` (empty — must be set)     |
    /// | `RUBBERDUX_LLM_BASE_URL` | `https://api.anthropic.com`    |
    /// | `RUBBERDUX_LLM_MODEL`    | `claude-opus-4-5-20251101`     |
    pub fn from_env() -> Result<Self, Error> {
        let base_url = std::env::var("RUBBERDUX_LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        let api_key = std::env::var("RUBBERDUX_LLM_API_KEY").unwrap_or_default();
        let model = std::env::var("RUBBERDUX_LLM_MODEL")
            .unwrap_or_else(|_| "claude-opus-4-5-20251101".into());

        let mut builder = reqwest::ClientBuilder::new();
        if let Ok(user_agent) = std::env::var("RUBBERDUX_LLM_USER_AGENT") {
            builder = builder.user_agent(user_agent);
        }
        let http = builder.build()?;

        Ok(Self {
            http,
            base_url,
            api_key,
            model,
        })
    }

    /// The configured model alias (useful for logging before a call is made).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// POST the assembled request body to `/v1/messages` and parse the response
    /// into the assistant content blocks and call metadata.
    ///
    /// `request_body` is the value produced by `MessageBuilder::build()`.
    /// `ModelMeta.model_id` carries the EFFECTIVE model the provider reports in
    /// its response — it may differ from the configured alias when the provider
    /// resolves routing internally (Inv 3: replay never re-resolves a live table).
    pub async fn call(&self, request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        let url = messages_url(&self.base_url);

        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ProviderApi {
                status: status.as_u16(),
                body,
            });
        }

        let raw: ApiResponse = response.json().await?;

        let stop_reason = match raw.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            Some("pause_turn") => StopReason::PauseTurn,
            other => {
                return Err(Error::Provider(format!(
                    "unrecognised stop_reason from provider: {:?}",
                    other
                )));
            }
        };

        let reasoning = if raw
            .content
            .iter()
            .any(|b| matches!(b, Block::Reasoning { .. }))
        {
            ReasoningPolicy::Echo
        } else {
            ReasoningPolicy::Drop
        };

        let meta = ModelMeta {
            usage: Usage {
                input_tokens: raw.usage.input_tokens,
                output_tokens: raw.usage.output_tokens,
            },
            // Effective model id from the response, not the requested alias
            // (Inv 3: recorded so replay never re-resolves a live table).
            model_id: raw.model,
            stop_reason,
            // P0: capability metadata is opaque; exact fields fixed by a later pass.
            capabilities: Capabilities(serde_json::json!({})),
            reasoning,
        };

        Ok((raw.content, meta))
    }
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Build the absolute messages endpoint URL from a configurable base.
///
/// Tolerates a trailing slash on `base` and avoids doubling the `/v1` segment
/// when the base already carries it (e.g. `https://api.kimi.com/coding/v1`).
/// A base without `/v1` (e.g. `https://api.anthropic.com`) gets `/v1/messages`
/// appended as usual.
fn messages_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{}/messages", trimmed)
    } else {
        format!("{}/v1/messages", trimmed)
    }
}

// ---------------------------------------------------------------------------
// Wire types — Anthropic /v1/messages response shape
// ---------------------------------------------------------------------------

/// The top-level Anthropic `/v1/messages` response. Only the fields that map
/// to `(Vec<Block>, ModelMeta)` are listed; unknown fields are ignored.
#[derive(Deserialize)]
struct ApiResponse {
    /// Effective model id reported by the provider. May differ from the
    /// requested alias when the provider resolves routing internally.
    model: String,
    content: Vec<Block>,
    stop_reason: Option<String>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- messages_url ----------------------------------------------------------

    #[test]
    fn messages_url_base_already_has_v1_segment() {
        assert_eq!(
            messages_url("https://api.kimi.com/coding/v1"),
            "https://api.kimi.com/coding/v1/messages"
        );
    }

    #[test]
    fn messages_url_base_with_v1_and_trailing_slash() {
        assert_eq!(
            messages_url("https://api.kimi.com/coding/v1/"),
            "https://api.kimi.com/coding/v1/messages"
        );
    }

    #[test]
    fn messages_url_base_without_v1_segment() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    /// Parse a fixed recorded Anthropic `/v1/messages` response into
    /// `(Vec<Block>, ModelMeta)` and assert the blocks and meta fields.
    #[test]
    fn parse_recorded_response_into_blocks_and_meta() {
        let recorded = json!({
            "id": "msg_01XFDUDYJgAACzvnptvVoYEL",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-5-20251101",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "Let me reason through this.",
                    "signature": "sig-abc123"
                },
                {
                    "type": "text",
                    "text": "The answer is 42."
                }
            ],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50
            }
        });

        let api: ApiResponse =
            serde_json::from_value(recorded).expect("deserialise recorded response");

        // Block shapes
        assert_eq!(api.content.len(), 2);
        match &api.content[0] {
            Block::Reasoning { text, signature } => {
                assert_eq!(text, "Let me reason through this.");
                assert_eq!(signature, "sig-abc123");
            }
            other => panic!("expected Reasoning block, got {other:?}"),
        }
        match &api.content[1] {
            Block::Text { text } => assert_eq!(text, "The answer is 42."),
            other => panic!("expected Text block, got {other:?}"),
        }

        // Meta assembly — mirrors the logic in `call()`
        let stop_reason = StopReason::EndTurn;
        let reasoning = if api.content.iter().any(|b| matches!(b, Block::Reasoning { .. })) {
            ReasoningPolicy::Echo
        } else {
            ReasoningPolicy::Drop
        };
        let meta = ModelMeta {
            usage: Usage {
                input_tokens: api.usage.input_tokens,
                output_tokens: api.usage.output_tokens,
            },
            model_id: api.model.clone(),
            stop_reason,
            capabilities: Capabilities(serde_json::json!({})),
            reasoning,
        };

        assert_eq!(meta.model_id, "claude-opus-4-5-20251101");
        assert_eq!(meta.usage.input_tokens, 100);
        assert_eq!(meta.usage.output_tokens, 50);
        assert!(matches!(meta.stop_reason, StopReason::EndTurn));
        // Reasoning blocks present → policy recorded as Echo
        assert!(matches!(meta.reasoning, ReasoningPolicy::Echo));
    }

    /// A text-only response (no thinking blocks) records ReasoningPolicy::Drop.
    #[test]
    fn text_only_response_records_drop_reasoning_policy() {
        let recorded = json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5",
            "content": [{"type": "text", "text": "Hello."}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let api: ApiResponse = serde_json::from_value(recorded).expect("deserialise");
        let reasoning = if api.content.iter().any(|b| matches!(b, Block::Reasoning { .. })) {
            ReasoningPolicy::Echo
        } else {
            ReasoningPolicy::Drop
        };
        assert!(matches!(reasoning, ReasoningPolicy::Drop));
    }

    /// `from_env` produces a client; defaults apply when variables are unset.
    #[test]
    fn from_env_defaults_when_vars_absent() {
        // Capture and clear the vars so defaults take effect.
        let saved_url = std::env::var("RUBBERDUX_LLM_BASE_URL").ok();
        let saved_key = std::env::var("RUBBERDUX_LLM_API_KEY").ok();
        let saved_model = std::env::var("RUBBERDUX_LLM_MODEL").ok();

        // SAFETY: single-threaded test context; no other thread reads these vars.
        unsafe {
            std::env::remove_var("RUBBERDUX_LLM_BASE_URL");
            std::env::remove_var("RUBBERDUX_LLM_API_KEY");
            std::env::remove_var("RUBBERDUX_LLM_MODEL");
        }

        let client = MessagesClient::from_env().expect("from_env should succeed");
        assert_eq!(client.base_url, "https://api.anthropic.com");
        assert_eq!(client.api_key, "");
        assert_eq!(client.model, "claude-opus-4-5-20251101");

        // Restore
        // SAFETY: single-threaded test context; no other thread reads these vars.
        unsafe {
            if let Some(v) = saved_url {
                std::env::set_var("RUBBERDUX_LLM_BASE_URL", v);
            }
            if let Some(v) = saved_key {
                std::env::set_var("RUBBERDUX_LLM_API_KEY", v);
            }
            if let Some(v) = saved_model {
                std::env::set_var("RUBBERDUX_LLM_MODEL", v);
            }
        }
    }
}
