use serde::{Deserialize, Serialize};

use super::super::tool::{FunctionDefinition, ToolDefinition};
use super::super::{Message, MoonshotClient, UserContent};

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFormat {
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    pub r#type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoice {
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    #[serde(default)]
    pub cached_tokens: usize,
}

impl MoonshotClient {
    pub async fn chat(
        &self,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> Result<ChatResponse, crate::error::Error> {
        // Tool definitions are assembled by the provider's tool_definitions() method.
        // The caller passes the complete tool list — no internal merging needed.
        let tools = tools.filter(|t| !t.is_empty());

        // $web_search requires thinking to be explicitly disabled
        let has_web_search = tools
            .as_ref()
            .map(|t| t.iter().any(|td| td.function.name == "$web_search"))
            .unwrap_or(false);

        let thinking = if has_web_search {
            Some(ThinkingConfig {
                r#type: "disabled".to_owned(),
            })
        } else {
            None
        };

        let request = ChatRequest {
            model: self.model().to_owned(),
            messages,
            temperature: Some(0.6),
            max_completion_tokens: None,
            tools,
            response_format: None,
            thinking,
        };

        if let Ok(json) = serde_json::to_string(&request) {
            log::info!("Chat API request JSON: {}", json);
        }

        let response = self
            .http()
            .post(self.url("/chat/completions"))
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(crate::error::Error::ProviderApi {
                status: status.as_u16(),
                body,
            });
        }

        let mut chat_response: ChatResponse = response.json().await?;

        // Ensure reasoning_content is present on assistant messages.
        // Kimi requires it when thinking mode is enabled, but some responses
        // (e.g. builtin $web_search) don't include it.
        for choice in &mut chat_response.choices {
            if let Message::Assistant {
                reasoning_content, ..
            } = &mut choice.message
            {
                if reasoning_content.is_none()
                    || reasoning_content.as_ref().is_some_and(|s| s.is_empty())
                {
                    *reasoning_content = Some("(tool call)".to_owned());
                }
            }
        }

        Ok(chat_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_request_serialization() {
        let request = ChatRequest {
            model: "kimi-for-coding".into(),
            messages: vec![
                Message::System {
                    content: "You are helpful.".into(),
                },
                Message::User {
                    content: UserContent::Text("Hello".into()),
                },
            ],
            temperature: None,
            max_completion_tokens: None,
            tools: None,
            response_format: None,
            thinking: None,
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "kimi-for-coding");
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        // Temperature should be absent when None
        assert!(json.get("temperature").is_none());
        assert!(json.get("tools").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn test_chat_response_deserialization() {
        let json = r#"{
            "id": "cmpl-test",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "kimi-for-coding",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "cached_tokens": 0
            }
        }"#;

        let response: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.usage.prompt_tokens, 10);
        assert_eq!(response.usage.completion_tokens, 5);
        assert_eq!(response.choices[0].finish_reason, "stop");
        assert!(matches!(
            &response.choices[0].message,
            Message::Assistant { content: Some(c), .. } if c == "Hello!"
        ));
    }

    /// Integration test: verifies that when web_search tool is present, thinking is
    /// properly disabled so the model actually calls $web_search instead of refusing.
    ///
    /// This test makes a real API call (costs tokens). Skip with `cargo test -- --skip integration`
    /// if you want to avoid network calls.
    #[tokio::test]
    #[ignore = "makes real API call — run with `cargo test -- --ignored`"]
    async fn test_web_search_triggers_tool_calls() {
        let client = MoonshotClient::from_env();
        let tools = vec![ToolDefinition {
            r#type: "builtin_function".to_owned(),
            function: FunctionDefinition {
                name: "$web_search".to_owned(),
                description: Some("Search the web".to_owned()),
                parameters: None,
            },
        }];

        let messages = vec![
            Message::System {
                content: "You must use web search for current information.".into(),
            },
            Message::User {
                content: UserContent::Text("Latest Google news".into()),
            },
        ];

        let response = client.chat(messages, Some(tools)).await;
        assert!(response.is_ok(), "API call failed: {:?}", response.err());

        let response = response.unwrap();
        let choice = response.choices.first().expect("no choices in response");

        // The critical assertion: with thinking disabled, the model should call
        // the tool (finish_reason = tool_calls), not refuse (finish_reason = stop).
        assert_eq!(
            choice.finish_reason,
            "tool_calls",
            "Model refused to use web_search. Thinking may not have been disabled. \
             finish_reason={}, content={:?}",
            choice.finish_reason,
            choice.message.content_text()
        );

        assert!(
            choice.message.tool_calls().is_some(),
            "Expected tool_calls in response"
        );

        let tool_calls = choice.message.tool_calls().unwrap();
        assert!(
            tool_calls
                .iter()
                .any(|tc| tc.function.name == "$web_search"),
            "Expected $web_search in tool calls, got: {:?}",
            tool_calls
                .iter()
                .map(|tc| &tc.function.name)
                .collect::<Vec<_>>()
        );
    }
}
