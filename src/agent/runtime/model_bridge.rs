//! model_bridge — the host-path seam between the OpenAI-shaped message list the
//! host runtime speaks (`kimi_for_coding::Message` / `ChatResponse`) and the
//! neutral provider vocabulary ([`ModelRequest`] / [`ModelResponse`]).
//!
//! The host chat agent loop, App identity naming, and conversation clustering
//! all assemble their conversation as `kimi_for_coding::Message`s and consume a
//! `kimi_for_coding::api::chat::ChatResponse`. Routing them through the selected
//! [`crate::provider::ModelApi`] requires translating in both directions; these
//! two pure functions are that translation, kept in one cohesive home so the
//! conversion is not scattered across call sites. See the design under
//! `docs/agent/runtime/` and the provider model under `docs/provider/`.

use crate::provider::kimi_for_coding::api::chat::{ChatChoice, ChatResponse, Usage as KimiUsage};
use crate::provider::kimi_for_coding::tool::{FunctionCall, ToolCall, ToolDefinition};
use crate::provider::kimi_for_coding::{ContentPart, Message, UserContent};
use crate::provider::{
    ContentBlock, ContentMessage, ImageSource, Json, ModelRequest, ModelResponse, Role, Sampling,
    StopReason, ToolSpec,
};

/// Translate the host's OpenAI-shaped message list and offered tools into a
/// neutral [`ModelRequest`].
///
/// The leading `System` message(s) become [`ModelRequest::system`]; every other
/// message becomes a [`ContentMessage`] carrying explicit content blocks
/// (assistant `tool_calls` → [`ContentBlock::ToolUse`], assistant
/// `reasoning_content` → [`ContentBlock::Reasoning`], a `Tool` message →
/// [`ContentBlock::ToolResult`], inline images → [`ContentBlock::Image`]). Tool
/// declarations map to [`ToolSpec`]. Pure and side-effect free.
pub fn to_model_request(
    messages: &[Message],
    tools: Option<&[ToolDefinition]>,
    sampling: Sampling,
) -> ModelRequest {
    let mut system = String::new();
    let mut content_messages: Vec<ContentMessage> = Vec::new();

    for msg in messages {
        match msg {
            Message::System { content } => {
                if system.is_empty() {
                    system = content.clone();
                } else {
                    // More than one system message: join so none is lost.
                    system.push('\n');
                    system.push_str(content);
                }
            }
            Message::User { content } => content_messages.push(ContentMessage {
                role: Role::User,
                content: user_content_to_blocks(content),
            }),
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                // Reasoning first so the Anthropic dialect (where thinking blocks
                // must precede text) round-trips; signature is opaque/empty here.
                if let Some(reasoning) = reasoning_content
                    && !reasoning.is_empty()
                {
                    blocks.push(ContentBlock::Reasoning {
                        text: reasoning.clone(),
                        signature: String::new(),
                    });
                }
                if let Some(text) = content
                    && !text.is_empty()
                {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        blocks.push(ContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            input: parse_tool_arguments(&call.function.arguments),
                        });
                    }
                }
                content_messages.push(ContentMessage {
                    role: Role::Assistant,
                    content: blocks,
                });
            }
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => content_messages.push(ContentMessage {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: vec![ContentBlock::Text {
                        text: content.clone(),
                    }],
                    is_error: false,
                }],
            }),
        }
    }

    let tool_specs = tools.map(tooldefs_to_specs).unwrap_or_default();

    ModelRequest {
        system,
        messages: content_messages,
        tools: tool_specs,
        sampling,
    }
}

/// Translate a neutral [`ModelResponse`] back into the host's `ChatResponse`.
///
/// `Text` blocks fold into the assistant `content`, `Reasoning` into
/// `reasoning_content`, and `ToolUse` into `tool_calls`; the neutral
/// [`StopReason`] maps onto the OpenAI-shaped `finish_reason` the turn driver
/// branches on (`end_turn` → `"stop"`, `tool_use` → `"tool_calls"`, …).
pub fn from_model_response(resp: ModelResponse) -> ChatResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for (index, block) in resp.blocks.into_iter().enumerate() {
        match block {
            ContentBlock::Text { text } => text_parts.push(text),
            ContentBlock::Reasoning { text, .. } => reasoning_parts.push(text),
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                index: Some(index as u32),
                id,
                r#type: "function".to_owned(),
                function: FunctionCall {
                    name,
                    arguments: serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned()),
                },
                depends_on: None,
            }),
            // Tool results and images are not part of an assistant response.
            ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };
    let reasoning_content = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };
    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    let message = Message::Assistant {
        content,
        reasoning_content,
        tool_calls,
        partial: None,
    };

    let usage = KimiUsage {
        prompt_tokens: resp.usage.input_tokens as usize,
        completion_tokens: resp.usage.output_tokens as usize,
        total_tokens: (resp.usage.input_tokens + resp.usage.output_tokens) as usize,
        cached_tokens: 0,
    };

    ChatResponse {
        // The neutral response carries the effective model id, not a response id;
        // it is used only for logging/recording downstream.
        id: resp.model_id,
        choices: vec![ChatChoice {
            message,
            finish_reason: finish_reason_from_stop(resp.stop_reason).to_owned(),
        }],
        usage,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a neutral [`StopReason`] onto the OpenAI-shaped `finish_reason` string
/// the turn driver compares against (`"stop"` means the turn is done).
fn finish_reason_from_stop(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "stop",
        StopReason::ToolUse => "tool_calls",
        StopReason::MaxTokens => "length",
        // No host finish_reason for a refusal/pause; treat the turn as complete.
        StopReason::Refusal | StopReason::PauseTurn => "stop",
    }
}

/// Map the host's user content (plain text or multimodal parts) into neutral
/// content blocks. Video parts have no neutral block and are dropped.
fn user_content_to_blocks(content: &UserContent) -> Vec<ContentBlock> {
    match content {
        UserContent::Text(text) => vec![ContentBlock::Text { text: text.clone() }],
        UserContent::Parts(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        blocks.push(ContentBlock::Text { text: text.clone() })
                    }
                    ContentPart::ImageUrl { image_url } => blocks.push(ContentBlock::Image {
                        source: image_source_from_url(&image_url.url),
                    }),
                    // The neutral vocabulary models images only; skip video.
                    ContentPart::VideoUrl { .. } => {}
                }
            }
            blocks
        }
    }
}

/// Parse a host image URL into a neutral [`ImageSource`]: a `data:` URI becomes
/// inline base64; anything else (incl. `ms://`, `http(s)://`) is kept as a URL.
fn image_source_from_url(url: &str) -> ImageSource {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some(comma) = rest.find(',')
    {
        let header = &rest[..comma];
        let media_type = header.split(';').next().unwrap_or("image/jpeg").to_owned();
        let data = rest[comma + 1..].to_owned();
        return ImageSource::Base64 { media_type, data };
    }
    ImageSource::Url {
        url: url.to_owned(),
    }
}

/// Parse a tool call's serialised `arguments` string into a JSON value. Empty
/// arguments become `{}`; non-JSON text is preserved as a JSON string.
fn parse_tool_arguments(arguments: &str) -> Json {
    if arguments.trim().is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str::<Json>(arguments).unwrap_or_else(|_| Json::String(arguments.to_owned()))
}

/// Map the host's tool declarations into neutral [`ToolSpec`]s.
fn tooldefs_to_specs(tools: &[ToolDefinition]) -> Vec<ToolSpec> {
    tools
        .iter()
        .map(|t| ToolSpec {
            name: t.function.name.clone(),
            description: t.function.description.clone().unwrap_or_default(),
            input_schema: t
                .function
                .parameters
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::kimi_for_coding::tool::FunctionDefinition;
    use crate::provider::{Effort, ReasoningPolicy, Usage};

    fn sampling() -> Sampling {
        Sampling {
            model: "test-model".to_owned(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// A representative conversation: system + user + assistant (with reasoning
    /// and a tool_call) + tool result. Asserts the `ModelRequest` shape.
    #[test]
    fn to_model_request_maps_a_full_conversation() {
        let messages = vec![
            Message::System {
                content: "You are helpful.".to_owned(),
            },
            Message::User {
                content: UserContent::Text("What's the weather?".to_owned()),
            },
            Message::Assistant {
                content: Some("Let me check.".to_owned()),
                reasoning_content: Some("thinking".to_owned()),
                tool_calls: Some(vec![ToolCall {
                    index: Some(0),
                    id: "call_1".to_owned(),
                    r#type: "function".to_owned(),
                    function: FunctionCall {
                        name: "get_weather".to_owned(),
                        arguments: r#"{"city":"Paris"}"#.to_owned(),
                    },
                    depends_on: None,
                }]),
                partial: None,
            },
            Message::Tool {
                tool_call_id: "call_1".to_owned(),
                name: None,
                content: "Sunny, 21C".to_owned(),
            },
        ];
        let tools = vec![ToolDefinition {
            r#type: "function".to_owned(),
            function: FunctionDefinition {
                name: "get_weather".to_owned(),
                description: Some("Look up the weather".to_owned()),
                parameters: Some(serde_json::json!({"type": "object"})),
            },
        }];

        let req = to_model_request(&messages, Some(&tools), sampling());

        // Leading system message becomes ModelRequest.system.
        assert_eq!(req.system, "You are helpful.");
        // user + assistant + tool = 3 content messages.
        assert_eq!(req.messages.len(), 3);

        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(
            req.messages[0].content,
            vec![ContentBlock::Text {
                text: "What's the weather?".to_owned()
            }]
        );

        // Assistant: reasoning, then text, then tool_use.
        assert_eq!(req.messages[1].role, Role::Assistant);
        assert_eq!(req.messages[1].content.len(), 3);
        assert!(matches!(
            &req.messages[1].content[0],
            ContentBlock::Reasoning { text, signature } if text == "thinking" && signature.is_empty()
        ));
        assert!(matches!(
            &req.messages[1].content[1],
            ContentBlock::Text { text } if text == "Let me check."
        ));
        match &req.messages[1].content[2] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "get_weather");
                assert_eq!(input, &serde_json::json!({"city": "Paris"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        // Tool message becomes a Tool-role ToolResult.
        assert_eq!(req.messages[2].role, Role::Tool);
        match &req.messages[2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert!(!is_error);
                assert_eq!(
                    content,
                    &vec![ContentBlock::Text {
                        text: "Sunny, 21C".to_owned()
                    }]
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        // Tool declaration mapped to a ToolSpec.
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "get_weather");
        assert_eq!(req.tools[0].description, "Look up the weather");
        assert_eq!(req.tools[0].input_schema, serde_json::json!({"type": "object"}));

        assert_eq!(req.sampling.model, "test-model");
    }

    /// `from_model_response` folds blocks into the assistant message and maps the
    /// stop reason to the host `finish_reason`.
    #[test]
    fn from_model_response_maps_blocks_and_finish_reason() {
        let resp = ModelResponse {
            blocks: vec![
                ContentBlock::Reasoning {
                    text: "thought".to_owned(),
                    signature: "sig".to_owned(),
                },
                ContentBlock::Text {
                    text: "Calling a tool.".to_owned(),
                },
                ContentBlock::ToolUse {
                    id: "call_9".to_owned(),
                    name: "lookup".to_owned(),
                    input: serde_json::json!({"q": "x"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
            },
            model_id: "effective-model".to_owned(),
            reasoning: ReasoningPolicy::Echo,
            capabilities: serde_json::json!({}),
        };

        let chat = from_model_response(resp);

        assert_eq!(chat.id, "effective-model");
        assert_eq!(chat.usage.prompt_tokens, 11);
        assert_eq!(chat.usage.completion_tokens, 7);
        assert_eq!(chat.usage.total_tokens, 18);
        assert_eq!(chat.choices.len(), 1);

        let choice = &chat.choices[0];
        // tool_use stop reason → "tool_calls" finish reason.
        assert_eq!(choice.finish_reason, "tool_calls");
        match &choice.message {
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                assert_eq!(content.as_deref(), Some("Calling a tool."));
                assert_eq!(reasoning_content.as_deref(), Some("thought"));
                let calls = tool_calls.as_ref().expect("tool_calls present");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_9");
                assert_eq!(calls[0].function.name, "lookup");
                assert_eq!(
                    serde_json::from_str::<Json>(&calls[0].function.arguments).unwrap(),
                    serde_json::json!({"q": "x"})
                );
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    /// An end-of-turn text response maps to `finish_reason == "stop"` with no
    /// tool calls — the turn driver's "model done" condition.
    #[test]
    fn from_model_response_text_only_is_stop() {
        let resp = ModelResponse {
            blocks: vec![ContentBlock::Text {
                text: "All done.".to_owned(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            model_id: "m".to_owned(),
            reasoning: ReasoningPolicy::Drop,
            capabilities: serde_json::json!({}),
        };

        let chat = from_model_response(resp);
        let choice = &chat.choices[0];
        assert_eq!(choice.finish_reason, "stop");
        assert_eq!(choice.message.content_text(), "All done.");
        assert!(choice.message.tool_calls().is_none());
    }

    /// A data-URI image round-trips to inline base64; a plain URL stays a URL.
    #[test]
    fn user_image_parts_map_to_image_blocks() {
        let messages = vec![Message::User {
            content: UserContent::Parts(vec![
                ContentPart::Text {
                    text: "look".to_owned(),
                },
                ContentPart::ImageUrl {
                    image_url: crate::provider::kimi_for_coding::MediaUrl {
                        url: "data:image/png;base64,QUJD".to_owned(),
                    },
                },
                ContentPart::ImageUrl {
                    image_url: crate::provider::kimi_for_coding::MediaUrl {
                        url: "https://img.test/a.png".to_owned(),
                    },
                },
            ]),
        }];

        let req = to_model_request(&messages, None, sampling());
        assert!(req.tools.is_empty());
        let blocks = &req.messages[0].content;
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "look"));
        assert!(matches!(
            &blocks[1],
            ContentBlock::Image { source: ImageSource::Base64 { media_type, data } }
                if media_type == "image/png" && data == "QUJD"
        ));
        assert!(matches!(
            &blocks[2],
            ContentBlock::Image { source: ImageSource::Url { url } } if url == "https://img.test/a.png"
        ));
    }
}
