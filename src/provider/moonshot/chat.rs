use serde::{Deserialize, Serialize};

use super::tool::ToolDefinition;
use super::{Message, MoonshotClient, UserContent};

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
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
        // Append platform builtin tools to the user-provided tools
        let tools = {
            let mut all_tools = tools.unwrap_or_default();
            all_tools.extend(Self::platform_builtins());
            if all_tools.is_empty() {
                None
            } else {
                Some(all_tools)
            }
        };

        let request = ChatRequest {
            model: self.model().to_owned(),
            messages,
            temperature: None,
            max_completion_tokens: None,
            tools,
            response_format: None,
            thinking: None,
        };

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
                if reasoning_content.is_none() {
                    *reasoning_content = Some(String::new());
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
        // Optional fields should be absent
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
}
