pub mod chat;
pub mod file;
pub mod partial;
pub mod token;
pub mod tool;

use serde::{Deserialize, Serialize};
use tool::ToolCall;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial: Option<bool>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn content_text(&self) -> &str {
        match self {
            Message::System { content } => content,
            Message::User { content } => content,
            Message::Assistant { content, .. } => content.as_deref().unwrap_or(""),
            Message::Tool { content, .. } => content,
        }
    }
}

pub struct MoonshotClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl MoonshotClient {
    pub fn from_env() -> Self {
        let base_url = std::env::var("RUBBERDUX_LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.moonshot.ai/v1".into());
        let api_key = std::env::var("RUBBERDUX_LLM_API_KEY").unwrap_or_default();
        let model =
            std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());

        let mut builder = reqwest::ClientBuilder::new();
        if let Ok(user_agent) = std::env::var("RUBBERDUX_LLM_USER_AGENT") {
            builder = builder.user_agent(user_agent);
        }
        let http = builder.build().expect("failed to build HTTP client");

        Self {
            http,
            base_url,
            api_key,
            model,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn build_messages(
        &self,
        system_prompt: &str,
        history: &[Message],
        user_input: &str,
    ) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(history.len() + 2);
        msgs.push(Message::System {
            content: system_prompt.to_owned(),
        });
        msgs.extend_from_slice(history);
        msgs.push(Message::User {
            content: user_input.to_owned(),
        });
        msgs
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    pub(crate) fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let sys = Message::System {
            content: "You are helpful.".into(),
        };
        let json = serde_json::to_value(&sys).unwrap();
        assert_eq!(json["role"], "system");
        assert_eq!(json["content"], "You are helpful.");

        let user = Message::User {
            content: "Hello".into(),
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Hello");

        let asst = Message::Assistant {
            content: Some("Hi!".into()),
            tool_calls: None,
            partial: None,
        };
        let json = serde_json::to_value(&asst).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "Hi!");
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("partial").is_none());

        let tool = Message::Tool {
            tool_call_id: "call_123".into(),
            content: "result".into(),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_123");
        assert_eq!(json["content"], "result");
    }

    #[test]
    fn test_build_messages_ordering() {
        let client = MoonshotClient {
            http: reqwest::Client::new(),
            base_url: "https://example.com/v1".into(),
            api_key: "test".into(),
            model: "test-model".into(),
        };

        let history = vec![
            Message::User {
                content: "First".into(),
            },
            Message::Assistant {
                content: Some("Reply".into()),
                tool_calls: None,
                partial: None,
            },
        ];

        let msgs = client.build_messages("System prompt", &history, "Second");

        assert_eq!(msgs.len(), 4);

        // First is system
        assert!(matches!(&msgs[0], Message::System { content } if content == "System prompt"));
        // History in middle
        assert!(matches!(&msgs[1], Message::User { content } if content == "First"));
        assert!(matches!(&msgs[2], Message::Assistant { content: Some(c), .. } if c == "Reply"));
        // Last is new user input
        assert!(matches!(&msgs[3], Message::User { content } if content == "Second"));
    }
}
