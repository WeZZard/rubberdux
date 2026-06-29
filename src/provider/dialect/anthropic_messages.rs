//! anthropic_messages — the Anthropic Messages dialect adapter.
//!
//! Implements [`ModelApi`] over the Anthropic `POST /v1/messages` surface: it
//! translates the neutral [`ModelRequest`] into the Anthropic wire body, parses
//! the `/v1/messages` response back into the neutral [`ModelResponse`], and
//! serves the provider's model list via `GET /v1/models`. The wire logic is
//! ported from the world's former `MessagesClient`; the neutral-vocabulary
//! translation is what makes it a dialect of [`ModelApi`] rather than a
//! world-specific client.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::provider::{
    AuthScheme, ContentBlock, Dialect, Effort, ImageSource, Json, ModelApi, ModelInfo,
    ModelRequest, ModelResponse, ReasoningPolicy, Role, StopReason, Usage,
};

/// The Anthropic Messages adapter. Holds the resolved configuration (HTTP
/// client, base URL, API key, configured model alias, auth scheme) a turn is
/// driven with.
pub struct AnthropicMessages {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    auth: AuthScheme,
}

impl AnthropicMessages {
    pub fn new(
        http: reqwest::Client,
        base_url: String,
        api_key: String,
        model: String,
        auth: AuthScheme,
    ) -> Self {
        Self {
            http,
            base_url,
            api_key,
            model,
            auth,
        }
    }

    /// The configured model alias.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The dialect this adapter speaks.
    pub fn dialect(&self) -> Dialect {
        Dialect::AnthropicMessages
    }

    /// Apply the configured authentication and the Anthropic version header to a
    /// request. Honours the stored [`AuthScheme`]: `AnthropicXApiKey` sends
    /// `x-api-key`; `Bearer` sends `Authorization: Bearer`. Either way the
    /// `anthropic-version` header is always set, as the Messages surface requires.
    fn apply_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = match self.auth {
            AuthScheme::AnthropicXApiKey => rb.header("x-api-key", &self.api_key),
            AuthScheme::Bearer => rb.header("authorization", format!("Bearer {}", self.api_key)),
        };
        rb.header("anthropic-version", "2023-06-01")
    }

    /// Build the Anthropic `/v1/messages` request body from a neutral request.
    /// `model`/`max_tokens`/`system` come from the request; each neutral
    /// [`ContentBlock`] is mapped to its Anthropic wire block, and `tools` is
    /// omitted entirely when empty (byte-identical to a tool-less call).
    fn build_request<'a>(&'a self, req: &'a ModelRequest) -> MessagesRequest<'a> {
        MessagesRequest {
            model: &req.sampling.model,
            max_tokens: req.sampling.max_tokens,
            system: &req.system,
            messages: req
                .messages
                .iter()
                .map(|m| WireMessage {
                    role: anthropic_role(m.role),
                    content: m.content.iter().map(to_wire_block).collect(),
                })
                .collect(),
            tools: req
                .tools
                .iter()
                .map(|t| WireTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.input_schema.clone(),
                })
                .collect(),
            thinking: ThinkingConfig { kind: "adaptive" },
            output_config: OutputConfig {
                effort: req.sampling.effort,
            },
        }
    }
}

impl ModelApi for AnthropicMessages {
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        Box::pin(async move {
            let url = messages_url(&self.base_url);
            let body = self.build_request(req);

            let rb = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .json(&body);
            let response = self.apply_auth(rb).send().await?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(Error::ProviderApi {
                    status: status.as_u16(),
                    body,
                });
            }

            let raw: ApiResponse = response.json().await?;
            parse_messages_response(raw)
        })
    }

    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        Box::pin(async move {
            let url = models_url(&self.base_url);
            let response = self.apply_auth(self.http.get(&url)).send().await?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(Error::ProviderApi {
                    status: status.as_u16(),
                    body,
                });
            }

            let raw: ModelsResponse = response.json().await?;
            Ok(raw
                .data
                .into_iter()
                .map(|e| ModelInfo {
                    id: e.id,
                    owned_by: e.owned_by,
                    context_length: e.context_length,
                })
                .collect())
        })
    }

    fn model(&self) -> &str {
        &self.model
    }
}

// ---------------------------------------------------------------------------
// Neutral ↔ Anthropic-wire translation
// ---------------------------------------------------------------------------

/// Map a neutral [`Role`] to the Anthropic message role. Anthropic carries only
/// `user`/`assistant`: tool results ride inside a user-role message and the
/// system prompt is a top-level field, so everything that is not `Assistant`
/// maps to `user`.
fn anthropic_role(role: Role) -> &'static str {
    match role {
        Role::Assistant => "assistant",
        Role::User | Role::Tool | Role::System => "user",
    }
}

/// Translate a neutral [`ContentBlock`] into its Anthropic wire block. The only
/// shape difference from the neutral serde form is `Reasoning` → `thinking`.
fn to_wire_block(block: &ContentBlock) -> WireBlock {
    match block {
        ContentBlock::Text { text } => WireBlock::Text { text: text.clone() },
        ContentBlock::Reasoning { text, signature } => WireBlock::Thinking {
            text: text.clone(),
            signature: signature.clone(),
        },
        ContentBlock::ToolUse { id, name, input } => WireBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => WireBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.iter().map(to_wire_block).collect(),
            is_error: *is_error,
        },
        ContentBlock::Image { source } => WireBlock::Image {
            source: source.clone(),
        },
    }
}

/// Translate an Anthropic wire block into a neutral [`ContentBlock`] (the
/// inverse of [`to_wire_block`]).
fn to_neutral_block(block: WireBlock) -> ContentBlock {
    match block {
        WireBlock::Text { text } => ContentBlock::Text { text },
        WireBlock::Thinking { text, signature } => ContentBlock::Reasoning { text, signature },
        WireBlock::ToolUse { id, name, input } => ContentBlock::ToolUse { id, name, input },
        WireBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ContentBlock::ToolResult {
            tool_use_id,
            content: content.into_iter().map(to_neutral_block).collect(),
            is_error,
        },
        WireBlock::Image { source } => ContentBlock::Image { source },
    }
}

/// Parse a deserialised `/v1/messages` response into a neutral [`ModelResponse`].
///
/// Mirrors the former `MessagesClient` mapping: `stop_reason` maps the Anthropic
/// vocabulary (`end_turn`→`EndTurn`, `tool_use`→`ToolUse`, `max_tokens`→
/// `MaxTokens`, `refusal`→`Refusal`, `pause_turn`→`PauseTurn`); an unrecognised
/// value is an [`Error`]. `model_id` is the EFFECTIVE model the provider reports.
/// `reasoning` is `Echo` when any reasoning block is present, else `Drop`.
/// `capabilities` is an empty opaque object (preserving the passthrough contract).
fn parse_messages_response(raw: ApiResponse) -> Result<ModelResponse, Error> {
    let stop_reason = match raw.stop_reason.as_deref() {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        Some("pause_turn") => StopReason::PauseTurn,
        other => {
            return Err(Error::Provider(format!(
                "unrecognised stop_reason from provider: {other:?}"
            )));
        }
    };

    let blocks: Vec<ContentBlock> = raw.content.into_iter().map(to_neutral_block).collect();

    let reasoning = if blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Reasoning { .. }))
    {
        ReasoningPolicy::Echo
    } else {
        ReasoningPolicy::Drop
    };

    Ok(ModelResponse {
        blocks,
        stop_reason,
        usage: Usage {
            input_tokens: raw.usage.input_tokens,
            output_tokens: raw.usage.output_tokens,
        },
        // Effective model id from the response, not the requested alias
        // (recorded so replay never re-resolves a live routing table).
        model_id: raw.model,
        reasoning,
        // Capability metadata is opaque; the world seam wraps it into its
        // Capabilities type. An empty object preserves the passthrough contract.
        capabilities: serde_json::json!({}),
    })
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Join `endpoint` onto a configurable base. Tolerates a trailing slash and
/// avoids doubling the `/v1` segment when the base already carries it (e.g.
/// `https://api.kimi.com/coding/v1`); a base without `/v1` (e.g.
/// `https://api.anthropic.com`) gets `/v1/{endpoint}` appended.
fn endpoint_url(base: &str, endpoint: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/{endpoint}")
    } else {
        format!("{trimmed}/v1/{endpoint}")
    }
}

/// The absolute `/v1/messages` endpoint URL.
fn messages_url(base: &str) -> String {
    endpoint_url(base, "messages")
}

/// The absolute `/v1/models` endpoint URL.
fn models_url(base: &str) -> String {
    endpoint_url(base, "models")
}

// ---------------------------------------------------------------------------
// Wire types — Anthropic /v1/messages request and response shapes
// ---------------------------------------------------------------------------

/// The Anthropic `/v1/messages` request body. Field order and naming are
/// explicit; `tools` is omitted when empty so a tool-less call is byte-identical
/// to a pre-tools call.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    thinking: ThinkingConfig,
    output_config: OutputConfig,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<WireBlock>,
}

/// One tool declaration in the Anthropic `tools` wire shape.
#[derive(Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: Json,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct OutputConfig {
    effort: Effort,
}

/// An Anthropic content block, shaped to the `/v1/messages` content schema. Used
/// for both BUILDING the request (serialise) and PARSING the response
/// (deserialise). `Reasoning` is carried as a `thinking` block; `Image` reuses
/// the neutral [`ImageSource`], whose serde form already matches the Anthropic
/// image source shape.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock {
    Text {
        text: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(rename = "thinking")]
        text: String,
        #[serde(default)]
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Json,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<WireBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
}

/// The top-level Anthropic `/v1/messages` response. Only the fields that map to
/// a [`ModelResponse`] are listed; unknown fields are ignored.
#[derive(Deserialize)]
struct ApiResponse {
    /// Effective model id reported by the provider. May differ from the
    /// requested alias when the provider resolves routing internally.
    model: String,
    content: Vec<WireBlock>,
    stop_reason: Option<String>,
    usage: ApiUsage,
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

/// The Anthropic `GET /v1/models` response. OpenAI-compatible endpoints may also
/// populate `owned_by`/`context_length`; both are optional.
#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    owned_by: Option<String>,
    #[serde(default)]
    context_length: Option<u32>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ContentMessage, Sampling, ToolSpec};
    use serde_json::json;

    // -- messages_url (ported from model_client.rs) ----------------------------

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

    #[test]
    fn models_url_appends_models_endpoint() {
        assert_eq!(
            models_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_url("https://api.kimi.com/coding/v1/"),
            "https://api.kimi.com/coding/v1/models"
        );
    }

    // -- helpers ---------------------------------------------------------------

    fn test_adapter(base_url: String) -> AnthropicMessages {
        AnthropicMessages::new(
            reqwest::Client::new(),
            base_url,
            "test-key".into(),
            "configured-alias".into(),
            AuthScheme::AnthropicXApiKey,
        )
    }

    fn sample_request() -> ModelRequest {
        ModelRequest {
            system: "you are helpful".into(),
            messages: vec![ContentMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            }],
            tools: vec![ToolSpec {
                name: "set_value".into(),
                description: "set a value".into(),
                input_schema: json!({ "type": "object" }),
            }],
            sampling: Sampling {
                model: "req-model".into(),
                max_tokens: 256,
                effort: Effort::High,
            },
        }
    }

    // -- V3.1 — request body building -----------------------------------------

    /// A neutral `ModelRequest` maps to the canonical Anthropic `/v1/messages`
    /// wire shape: `Reasoning` → `thinking`, `tool_use`/`tool_result` blocks,
    /// `thinking`/`output_config`, and a `tools` declaration array.
    #[test]
    fn build_request_maps_neutral_to_anthropic_wire_shape() {
        let adapter = test_adapter("https://api.anthropic.com".into());
        let req = ModelRequest {
            system: "sys".into(),
            messages: vec![
                ContentMessage {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Reasoning {
                            text: "let me think".into(),
                            signature: "sig-abc".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "tu_1".into(),
                            name: "get_weather".into(),
                            input: json!({ "city": "Paris" }),
                        },
                    ],
                },
                ContentMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "tu_1".into(),
                        content: vec![ContentBlock::Text {
                            text: "sunny".into(),
                        }],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "get_weather".into(),
                description: "weather".into(),
                input_schema: json!({ "type": "object" }),
            }],
            sampling: Sampling {
                model: "claude-x".into(),
                max_tokens: 1024,
                effort: Effort::High,
            },
        };

        let body = serde_json::to_value(adapter.build_request(&req)).expect("serialise body");

        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["system"], "sys");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        // Reasoning serialises as an Anthropic `thinking` block.
        assert_eq!(messages[0]["content"][0]["type"], "thinking");
        assert_eq!(messages[0]["content"][0]["thinking"], "let me think");
        assert_eq!(messages[0]["content"][0]["signature"], "sig-abc");
        assert_eq!(messages[0]["content"][1]["type"], "tool_use");
        assert_eq!(messages[0]["content"][1]["id"], "tu_1");
        // ToolResult rides inside the user message (Anthropic shape).
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(messages[1]["content"][0]["content"][0]["type"], "text");

        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "get_weather");
        assert!(tools[0]["description"].is_string());
    }

    /// A tool-less call omits the `tools` key entirely (byte-identical to a
    /// pre-tools request body — replay/fingerprint stability).
    #[test]
    fn build_request_omits_tools_when_empty() {
        let adapter = test_adapter("https://api.anthropic.com".into());
        let req = ModelRequest {
            system: "sys".into(),
            messages: vec![],
            tools: vec![],
            sampling: Sampling {
                model: "m".into(),
                max_tokens: 8,
                effort: Effort::Low,
            },
        };
        let body = serde_json::to_value(adapter.build_request(&req)).expect("serialise body");
        assert!(
            body.get("tools").is_none(),
            "a tool-less call must omit the `tools` key entirely"
        );
    }

    // -- V3.2 — response parsing (ported from model_client.rs) -----------------

    /// Parse a fixed recorded Anthropic `/v1/messages` response (thinking + text)
    /// into a neutral `ModelResponse` and assert the blocks and meta fields.
    #[test]
    fn parse_recorded_response_into_neutral_blocks_and_meta() {
        let recorded = json!({
            "id": "msg_01XFDUDYJgAACzvnptvVoYEL",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-5-20251101",
            "content": [
                { "type": "thinking", "thinking": "Let me reason through this.", "signature": "sig-abc123" },
                { "type": "text", "text": "The answer is 42." }
            ],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": 100, "output_tokens": 50 }
        });

        let raw: ApiResponse =
            serde_json::from_value(recorded).expect("deserialise recorded response");
        let resp = parse_messages_response(raw).expect("parse into ModelResponse");

        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            ContentBlock::Reasoning { text, signature } => {
                assert_eq!(text, "Let me reason through this.");
                assert_eq!(signature, "sig-abc123");
            }
            other => panic!("expected Reasoning block, got {other:?}"),
        }
        match &resp.blocks[1] {
            ContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
            other => panic!("expected Text block, got {other:?}"),
        }

        assert_eq!(resp.model_id, "claude-opus-4-5-20251101");
        assert_eq!(resp.usage.input_tokens, 100);
        assert_eq!(resp.usage.output_tokens, 50);
        assert!(matches!(resp.stop_reason, StopReason::EndTurn));
        // Reasoning blocks present → policy recorded as Echo.
        assert!(matches!(resp.reasoning, ReasoningPolicy::Echo));
        // Capability metadata is an empty opaque object.
        assert_eq!(resp.capabilities, json!({}));
    }

    /// A text-only response (no thinking blocks) records ReasoningPolicy::Drop.
    #[test]
    fn text_only_response_records_drop_reasoning_policy() {
        let recorded = json!({
            "id": "msg_02",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5",
            "content": [ { "type": "text", "text": "Hello." } ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        });

        let raw: ApiResponse = serde_json::from_value(recorded).expect("deserialise");
        let resp = parse_messages_response(raw).expect("parse");
        assert!(matches!(resp.reasoning, ReasoningPolicy::Drop));
    }

    // -- V3.3 — unrecognised stop_reason errors --------------------------------

    #[test]
    fn unrecognised_stop_reason_errors() {
        let recorded = json!({
            "model": "m",
            "content": [ { "type": "text", "text": "x" } ],
            "stop_reason": "who_knows",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let raw: ApiResponse = serde_json::from_value(recorded).expect("deserialise");
        let err = parse_messages_response(raw).expect_err("unrecognised stop_reason must error");
        assert!(matches!(err, Error::Provider(_)));
    }

    // -- V3.1/V3.2 — turn() happy path over a mock server ----------------------

    #[tokio::test]
    async fn turn_posts_messages_with_headers_and_parses_response() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // V3.1 — x-api-key + anthropic-version headers are sent.
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            // V3.1 — the body is built from the neutral request.
            .and(body_partial_json(json!({
                "model": "req-model",
                "max_tokens": 256,
                "system": "you are helpful",
                "thinking": { "type": "adaptive" },
                "output_config": { "effort": "high" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4-5-20251101",
                "content": [
                    { "type": "thinking", "thinking": "reasoning...", "signature": "sig-1" },
                    { "type": "text", "text": "hi there" }
                ],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 7, "output_tokens": 3 }
            })))
            .mount(&mock_server)
            .await;

        let adapter = test_adapter(mock_server.uri());
        let resp = adapter.turn(&sample_request()).await.expect("turn ok");

        assert_eq!(resp.model_id, "claude-opus-4-5-20251101");
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.usage.output_tokens, 3);
        assert!(matches!(resp.stop_reason, StopReason::EndTurn));
        assert!(matches!(resp.reasoning, ReasoningPolicy::Echo));
        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            ContentBlock::Reasoning { text, signature } => {
                assert_eq!(text, "reasoning...");
                assert_eq!(signature, "sig-1");
            }
            other => panic!("expected Reasoning block, got {other:?}"),
        }
        match &resp.blocks[1] {
            ContentBlock::Text { text } => assert_eq!(text, "hi there"),
            other => panic!("expected Text block, got {other:?}"),
        }
    }

    // -- V3.3 — non-2xx maps to Error::ProviderApi -----------------------------

    #[tokio::test]
    async fn turn_maps_non_2xx_to_provider_api_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
            .mount(&mock_server)
            .await;

        let adapter = test_adapter(mock_server.uri());
        let err = adapter
            .turn(&sample_request())
            .await
            .expect_err("non-2xx must error");
        match err {
            Error::ProviderApi { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("upstream boom"));
            }
            other => panic!("expected ProviderApi, got {other:?}"),
        }
    }

    // -- V3.4 — list_models parses the Anthropic models list -------------------

    #[tokio::test]
    async fn list_models_parses_anthropic_models_list() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "id": "claude-opus-4-5-20251101", "type": "model", "display_name": "Claude Opus 4.5" },
                    { "id": "claude-haiku-4-5", "type": "model" }
                ],
                "has_more": false
            })))
            .mount(&mock_server)
            .await;

        let adapter = test_adapter(mock_server.uri());
        let models = adapter.list_models().await.expect("list ok");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-opus-4-5-20251101");
        assert_eq!(models[1].id, "claude-haiku-4-5");
        assert!(models[0].owned_by.is_none());
        assert!(models[0].context_length.is_none());
    }

    #[tokio::test]
    async fn list_models_maps_non_2xx_to_provider_api_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .mount(&mock_server)
            .await;

        let adapter = test_adapter(mock_server.uri());
        let err = adapter
            .list_models()
            .await
            .expect_err("non-2xx must error");
        match err {
            Error::ProviderApi { status, body } => {
                assert_eq!(status, 503);
                assert!(body.contains("unavailable"));
            }
            other => panic!("expected ProviderApi, got {other:?}"),
        }
    }
}
