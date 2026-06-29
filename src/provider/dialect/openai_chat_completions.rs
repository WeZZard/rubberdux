//! openai_chat_completions — the generic OpenAI Chat Completions dialect adapter.
//!
//! Implements [`ModelApi`] by translating the neutral [`ModelRequest`] into an
//! OpenAI `POST /chat/completions` body, parsing the response back into a neutral
//! [`ModelResponse`], and listing models via `GET /v1/models`. This is the
//! protocol-only transport every OpenAI-shaped provider shares; provider-specific
//! quirks (e.g. Kimi's `$web_search`, `thinking`, temperature-lock, `ms://`
//! upload) live in their provider module, not here.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::provider::{
    AuthScheme, ContentBlock, Dialect, ImageSource, Json, ModelApi, ModelInfo, ModelRequest,
    ModelResponse, ReasoningPolicy, Role, StopReason, ToolSpec, Usage,
};

/// The OpenAI Chat Completions adapter. Holds the resolved configuration the
/// transport drives each call from.
pub struct OpenAiChatCompletions {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    auth: AuthScheme,
}

impl OpenAiChatCompletions {
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
        Dialect::OpenAiChatCompletions
    }
}

impl ModelApi for OpenAiChatCompletions {
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        Box::pin(async move {
            let body = build_chat_request(req);
            let url = chat_completions_url(&self.base_url);
            let (header_name, header_value) = auth_header(self.auth, &self.api_key);

            let response = self
                .http
                .post(&url)
                .header(header_name, header_value)
                .header("content-type", "application/json")
                .json(&body)
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

            let raw: ChatResponseBody = response.json().await?;
            parse_chat_response(raw)
        })
    }

    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        Box::pin(async move {
            let url = models_url(&self.base_url);
            let (header_name, header_value) = auth_header(self.auth, &self.api_key);

            let response = self
                .http
                .get(&url)
                .header(header_name, header_value)
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

            let raw: ModelsListBody = response.json().await?;
            Ok(raw
                .data
                .into_iter()
                .map(|m| ModelInfo {
                    id: m.id,
                    owned_by: m.owned_by,
                    context_length: m.context_length,
                })
                .collect())
        })
    }

    fn model(&self) -> &str {
        &self.model
    }
}

// ---------------------------------------------------------------------------
// URL + auth helpers
// ---------------------------------------------------------------------------

/// Build the absolute `/chat/completions` URL from a configurable base.
///
/// Tolerates a trailing slash and avoids doubling the `/v1` segment when the
/// base already carries it (e.g. `https://opencode.ai/zen/v1`); a base without
/// `/v1` (e.g. `https://api.openai.com`) gets `/v1/chat/completions` appended.
/// Mirrors `messages_url` in `agent/world/model_client.rs`.
fn chat_completions_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{}/chat/completions", trimmed)
    } else {
        format!("{}/v1/chat/completions", trimmed)
    }
}

/// Build the absolute `/models` URL from a configurable base, with the same
/// tolerant `/v1` joining as [`chat_completions_url`].
fn models_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{}/models", trimmed)
    } else {
        format!("{}/v1/models", trimmed)
    }
}

/// The `(header name, header value)` pair for the configured auth scheme.
/// OpenAI uses `Authorization: Bearer`; the `x-api-key` arm honours providers
/// reached over the OpenAI dialect that authenticate Anthropic-style.
fn auth_header(auth: AuthScheme, api_key: &str) -> (&'static str, String) {
    match auth {
        AuthScheme::Bearer => ("Authorization", format!("Bearer {}", api_key)),
        AuthScheme::AnthropicXApiKey => ("x-api-key", api_key.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Neutral → OpenAI request translation
// ---------------------------------------------------------------------------

/// Translate a neutral [`ModelRequest`] into the OpenAI `/chat/completions`
/// request body. Pure and side-effect free so the wire shape is unit-testable.
fn build_chat_request(req: &ModelRequest) -> ChatRequestBody {
    let max = req.sampling.max_tokens;
    ChatRequestBody {
        model: req.sampling.model.clone(),
        messages: translate_messages(req),
        // Both forms carry the same limit: `max_tokens` for classic
        // OpenAI-compatible endpoints, `max_completion_tokens` for newer ones.
        max_tokens: Some(max),
        max_completion_tokens: Some(max),
        tools: translate_tools(&req.tools),
    }
}

/// Translate the system prompt and conversation into OpenAI wire messages.
fn translate_messages(req: &ModelRequest) -> Vec<WireMessage> {
    let mut out = Vec::new();

    if !req.system.is_empty() {
        out.push(WireMessage {
            role: "system",
            content: Some(WireContent::Text(req.system.clone())),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for msg in &req.messages {
        match msg.role {
            // A neutral Tool message flattens to one OpenAI `tool` message per
            // `ToolResult` block, keyed by its originating tool-call id.
            Role::Tool => {
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        out.push(WireMessage {
                            role: "tool",
                            content: Some(WireContent::Text(flatten_blocks_to_text(content))),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
            Role::System | Role::User | Role::Assistant => {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool", // unreachable in this arm
                };

                let mut texts: Vec<String> = Vec::new();
                let mut images: Vec<WirePart> = Vec::new();
                let mut tool_calls: Vec<WireToolCall> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => texts.push(text.clone()),
                        // Reasoning has no OpenAI request field — dropped, never
                        // fabricated.
                        ContentBlock::Reasoning { .. } => {}
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(WireToolCall {
                                id: id.clone(),
                                r#type: "function",
                                function: WireFunctionCall {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input)
                                        .unwrap_or_else(|_| "{}".to_owned()),
                                },
                            });
                        }
                        // Tool results belong to the `Tool` role; ignore here.
                        ContentBlock::ToolResult { .. } => {}
                        ContentBlock::Image { source } => images.push(WirePart::ImageUrl {
                            image_url: WireImageUrl {
                                url: image_source_url(source),
                            },
                        }),
                    }
                }

                let joined = texts.join("\n");
                let content = if images.is_empty() {
                    if joined.is_empty() {
                        None
                    } else {
                        Some(WireContent::Text(joined))
                    }
                } else {
                    let mut parts = Vec::new();
                    if !joined.is_empty() {
                        parts.push(WirePart::Text { text: joined });
                    }
                    parts.extend(images);
                    Some(WireContent::Parts(parts))
                };

                out.push(WireMessage {
                    role,
                    content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                });
            }
        }
    }

    out
}

/// Translate neutral tool declarations into OpenAI function tools. `None` when
/// no tools are offered so the field is omitted from the wire body.
fn translate_tools(tools: &[ToolSpec]) -> Option<Vec<WireTool>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|t| WireTool {
                r#type: "function",
                function: WireToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect(),
    )
}

/// Flatten nested content blocks (a tool result's body) to a single text string.
/// Non-text blocks that have no textual form (images, tool-use) are skipped.
fn flatten_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::Reasoning { text, .. } => parts.push(text.clone()),
            ContentBlock::ToolResult { content, .. } => {
                parts.push(flatten_blocks_to_text(content))
            }
            ContentBlock::ToolUse { .. } | ContentBlock::Image { .. } => {}
        }
    }
    parts.join("\n")
}

/// Render a neutral image source as an OpenAI `image_url` value (a data URI for
/// inline base64, or the URL as-is).
fn image_source_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Base64 { media_type, data } => {
            format!("data:{};base64,{}", media_type, data)
        }
        ImageSource::Url { url } => url.clone(),
    }
}

// ---------------------------------------------------------------------------
// OpenAI → neutral response translation
// ---------------------------------------------------------------------------

/// Parse a `/chat/completions` response into a neutral [`ModelResponse`].
fn parse_chat_response(raw: ChatResponseBody) -> Result<ModelResponse, Error> {
    let choice = raw
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| Error::Provider("openai chat response had no choices".to_owned()))?;

    let mut blocks: Vec<ContentBlock> = Vec::new();
    if let Some(text) = choice.message.content
        && !text.is_empty()
    {
        blocks.push(ContentBlock::Text { text });
    }
    if let Some(tool_calls) = choice.message.tool_calls {
        for tc in tool_calls {
            blocks.push(ContentBlock::ToolUse {
                id: tc.id,
                name: tc.function.name,
                input: parse_arguments(&tc.function.arguments),
            });
        }
    }

    let usage = raw
        .usage
        .map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        })
        .unwrap_or_default();

    Ok(ModelResponse {
        blocks,
        stop_reason: map_finish_reason(choice.finish_reason.as_deref()),
        usage,
        // Effective model id the provider reports — may differ from the
        // requested alias when routing resolves internally.
        model_id: raw.model,
        // OpenAI Chat Completions has no signed reasoning to round-trip.
        reasoning: ReasoningPolicy::Drop,
        capabilities: serde_json::json!({}),
    })
}

/// Map an OpenAI `finish_reason` to a neutral [`StopReason`].
///
/// `"stop"` → `EndTurn`; `"tool_calls"` (and legacy `"function_call"`) →
/// `ToolUse`; `"length"` → `MaxTokens`; `"content_filter"` → `Refusal`. An
/// absent or unrecognised reason is treated as a completed turn (`EndTurn`):
/// the model produced output, so the turn is taken as done rather than erroring.
fn map_finish_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

/// Parse a tool call's `arguments` (an OpenAI-serialised JSON string) into a
/// JSON value. Empty arguments become `{}`; non-JSON text is preserved as a
/// JSON string rather than discarded.
fn parse_arguments(arguments: &str) -> Json {
    if arguments.trim().is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str::<Json>(arguments).unwrap_or_else(|_| Json::String(arguments.to_owned()))
}

// ---------------------------------------------------------------------------
// Wire types — OpenAI /chat/completions request shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ChatRequestBody {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool>>,
}

#[derive(Debug, Clone, Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WirePart {
    Text { text: String },
    ImageUrl { image_url: WireImageUrl },
}

#[derive(Debug, Clone, Serialize)]
struct WireImageUrl {
    url: String,
}

#[derive(Debug, Clone, Serialize)]
struct WireToolCall {
    id: String,
    r#type: &'static str,
    function: WireFunctionCall,
}

#[derive(Debug, Clone, Serialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize)]
struct WireTool {
    r#type: &'static str,
    function: WireToolFunction,
}

#[derive(Debug, Clone, Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: Json,
}

// ---------------------------------------------------------------------------
// Wire types — OpenAI /chat/completions + /models response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ChatResponseBody {
    /// Effective model id reported by the provider.
    #[serde(default)]
    model: String,
    choices: Vec<ResponseChoice>,
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseChoice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseToolCall {
    #[serde(default)]
    id: String,
    function: ResponseFunctionCall,
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseFunctionCall {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsListBody {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
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
    use crate::provider::{ContentMessage, Effort, Sampling};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sampling(model: &str) -> Sampling {
        Sampling {
            model: model.to_owned(),
            max_tokens: 256,
            effort: Effort::Medium,
        }
    }

    /// A request exercising every translated shape: system prompt, user text,
    /// assistant text + tool_use, a tool result, and an offered tool.
    fn rich_request() -> ModelRequest {
        ModelRequest {
            system: "You are helpful.".to_owned(),
            messages: vec![
                ContentMessage {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "What's the weather?".to_owned(),
                    }],
                },
                ContentMessage {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "Let me check.".to_owned(),
                        },
                        ContentBlock::Reasoning {
                            text: "internal".to_owned(),
                            signature: "sig".to_owned(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_1".to_owned(),
                            name: "get_weather".to_owned(),
                            input: json!({"city": "Paris"}),
                        },
                    ],
                },
                ContentMessage {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_owned(),
                        content: vec![ContentBlock::Text {
                            text: "Sunny, 21C".to_owned(),
                        }],
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "get_weather".to_owned(),
                description: "Look up the weather".to_owned(),
                input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            }],
            sampling: sampling("gpt-4o-mini"),
        }
    }

    // -- URL helpers -----------------------------------------------------------

    #[test]
    fn url_helpers_tolerate_v1_segment() {
        assert_eq!(
            chat_completions_url("https://opencode.ai/zen/v1"),
            "https://opencode.ai/zen/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://opencode.ai/zen/v1/"),
            "https://opencode.ai/zen/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(models_url("https://ollama.com/v1"), "https://ollama.com/v1/models");
        assert_eq!(models_url("https://api.openai.com"), "https://api.openai.com/v1/models");
    }

    #[test]
    fn auth_header_maps_scheme() {
        assert_eq!(
            auth_header(AuthScheme::Bearer, "sk-123"),
            ("Authorization", "Bearer sk-123".to_owned())
        );
        assert_eq!(
            auth_header(AuthScheme::AnthropicXApiKey, "key-9"),
            ("x-api-key", "key-9".to_owned())
        );
    }

    // -- V2.1 — build /chat/completions body from a neutral ModelRequest -------

    #[test]
    fn builds_chat_request_body_in_openai_shape() {
        let body = build_chat_request(&rich_request());
        let v = serde_json::to_value(&body).expect("serialise request body");

        // model from sampling.model
        assert_eq!(v["model"], "gpt-4o-mini");
        // token limit mapped onto both fields
        assert_eq!(v["max_tokens"], 256);
        assert_eq!(v["max_completion_tokens"], 256);

        let messages = v["messages"].as_array().expect("messages array");
        // system + user + assistant + tool = 4
        assert_eq!(messages.len(), 4);

        // System message from ModelRequest.system
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful.");

        // User text
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "What's the weather?");

        // Assistant: text content + tool_calls; reasoning dropped
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Let me check.");
        let tool_calls = messages[2]["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        // arguments is a JSON-encoded string
        let args: Json = serde_json::from_str(
            tool_calls[0]["function"]["arguments"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(args, json!({"city": "Paris"}));

        // Tool result flattened to a `tool` message keyed by tool_call_id
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[3]["content"], "Sunny, 21C");

        // tools in OpenAI function shape
        let tools = v["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["description"], "Look up the weather");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn omits_tools_and_system_when_absent() {
        let req = ModelRequest {
            system: String::new(),
            messages: vec![ContentMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: vec![],
            sampling: sampling("m"),
        };
        let v = serde_json::to_value(build_chat_request(&req)).unwrap();
        assert!(v.get("tools").is_none(), "tools omitted when empty");
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "no system message when system is empty");
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn image_block_becomes_content_parts() {
        let req = ModelRequest {
            system: String::new(),
            messages: vec![ContentMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "look".to_owned(),
                    },
                    ContentBlock::Image {
                        source: ImageSource::Url {
                            url: "https://img.test/a.png".to_owned(),
                        },
                    },
                ],
            }],
            tools: vec![],
            sampling: sampling("m"),
        };
        let v = serde_json::to_value(build_chat_request(&req)).unwrap();
        let parts = v["messages"][0]["content"].as_array().expect("parts array");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "https://img.test/a.png");
    }

    // -- V2.2 — parse /chat/completions response into neutral blocks -----------

    #[test]
    fn parses_response_into_neutral_blocks() {
        let raw: ChatResponseBody = serde_json::from_value(json!({
            "id": "cmpl-x",
            "model": "gpt-4o-mini-2024",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        }))
        .unwrap();

        let resp = parse_chat_response(raw).expect("parse");

        assert_eq!(resp.model_id, "gpt-4o-mini-2024");
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 11);
        assert_eq!(resp.usage.output_tokens, 7);
        assert_eq!(resp.reasoning, ReasoningPolicy::Drop);
        assert_eq!(resp.capabilities, json!({}));

        assert_eq!(resp.blocks.len(), 2);
        match &resp.blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
            other => panic!("expected Text, got {other:?}"),
        }
        match &resp.blocks[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_9");
                assert_eq!(name, "lookup");
                assert_eq!(input, &json!({"q": "x"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn maps_finish_reasons() {
        assert_eq!(map_finish_reason(Some("stop")), StopReason::EndTurn);
        assert_eq!(map_finish_reason(Some("tool_calls")), StopReason::ToolUse);
        assert_eq!(map_finish_reason(Some("function_call")), StopReason::ToolUse);
        assert_eq!(map_finish_reason(Some("length")), StopReason::MaxTokens);
        assert_eq!(map_finish_reason(Some("content_filter")), StopReason::Refusal);
        assert_eq!(map_finish_reason(None), StopReason::EndTurn);
        assert_eq!(map_finish_reason(Some("mystery")), StopReason::EndTurn);
    }

    #[test]
    fn parse_arguments_tolerates_empty_and_invalid() {
        assert_eq!(parse_arguments(""), json!({}));
        assert_eq!(parse_arguments("   "), json!({}));
        assert_eq!(parse_arguments("{\"a\":1}"), json!({"a": 1}));
        // Non-JSON arguments preserved as a string rather than discarded.
        assert_eq!(parse_arguments("not json"), json!("not json"));
    }

    // -- V2.1 + V2.2 — turn() against a mock server ----------------------------

    #[tokio::test]
    async fn turn_posts_and_parses_against_mock_server() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "cmpl-1",
                "model": "effective-model-7",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Hello there."},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
            })))
            .mount(&server)
            .await;

        let adapter = OpenAiChatCompletions::new(
            reqwest::Client::new(),
            server.uri(),
            "sk-test".into(),
            "configured-model".into(),
            AuthScheme::Bearer,
        );

        let req = rich_request();
        let resp = adapter.turn(&req).await.expect("turn");

        assert_eq!(resp.model_id, "effective-model-7");
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 3);
        assert_eq!(resp.usage.output_tokens, 2);
        match &resp.blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello there."),
            other => panic!("expected Text, got {other:?}"),
        }

        // Inspect the outbound request body and auth header (provider-tools
        // testing principle: assert the request that actually went out).
        let requests = server.received_requests().await.expect("recorded requests");
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert_eq!(
            sent.headers.get("authorization").map(|h| h.to_str().unwrap()),
            Some("Bearer sk-test")
        );
        let body: Json = serde_json::from_slice(&sent.body).expect("body json");
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
    }

    // -- V2.3 — list_models() against a mock server ----------------------------

    #[tokio::test]
    async fn list_models_parses_models_endpoint() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    {"id": "model-a", "owned_by": "acme", "context_length": 262144},
                    {"id": "model-b"}
                ]
            })))
            .mount(&server)
            .await;

        let adapter = OpenAiChatCompletions::new(
            reqwest::Client::new(),
            server.uri(),
            "sk-test".into(),
            "configured-model".into(),
            AuthScheme::Bearer,
        );

        let models = adapter.list_models().await.expect("list_models");
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0],
            ModelInfo {
                id: "model-a".to_owned(),
                owned_by: Some("acme".to_owned()),
                context_length: Some(262144),
            }
        );
        assert_eq!(
            models[1],
            ModelInfo {
                id: "model-b".to_owned(),
                owned_by: None,
                context_length: None,
            }
        );
    }

    // -- V2.4 — non-2xx maps to Error::ProviderApi { status, body } ------------

    #[tokio::test]
    async fn turn_maps_non_2xx_to_provider_api_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
            .mount(&server)
            .await;

        let adapter = OpenAiChatCompletions::new(
            reqwest::Client::new(),
            server.uri(),
            "sk-test".into(),
            "m".into(),
            AuthScheme::Bearer,
        );

        let err = adapter.turn(&rich_request()).await.expect_err("expected error");
        match err {
            Error::ProviderApi { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "upstream boom");
            }
            other => panic!("expected ProviderApi, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_maps_non_2xx_to_provider_api_error() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .mount(&server)
            .await;

        let adapter = OpenAiChatCompletions::new(
            reqwest::Client::new(),
            server.uri(),
            "sk-test".into(),
            "m".into(),
            AuthScheme::Bearer,
        );

        let err = adapter.list_models().await.expect_err("expected error");
        match err {
            Error::ProviderApi { status, body } => {
                assert_eq!(status, 503);
                assert_eq!(body, "unavailable");
            }
            other => panic!("expected ProviderApi, got {other:?}"),
        }
    }
}
