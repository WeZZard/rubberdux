pub mod chat;
pub mod file;
pub mod partial;
pub mod token;
pub mod tool;

use serde::{Deserialize, Serialize};
use tool::{FunctionDefinition, ToolCall, ToolDefinition};

use crate::channel::interpreter::{Attachment, InterpretedMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        content: Option<String>,
        #[serde(default)]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial: Option<bool>,
    },
    Tool {
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: MediaUrl,
    },
    VideoUrl {
        video_url: MediaUrl,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaUrl {
    pub url: String,
}

impl Message {
    pub fn content_text(&self) -> &str {
        match self {
            Message::System { content } => content,
            Message::User { content } => match content {
                UserContent::Text(t) => t,
                UserContent::Parts(parts) => {
                    for part in parts {
                        if let ContentPart::Text { text } = part {
                            return text;
                        }
                    }
                    ""
                }
            },
            Message::Assistant { content, .. } => content.as_deref().unwrap_or(""),
            Message::Tool { content, .. } => content,
        }
    }

    pub fn tool_calls(&self) -> Option<&Vec<ToolCall>> {
        match self {
            Message::Assistant { tool_calls, .. } => tool_calls.as_ref(),
            _ => None,
        }
    }

    pub fn from_interpreted(interpreted: &InterpretedMessage) -> Self {
        if interpreted.attachments.is_empty() {
            return Message::User {
                content: UserContent::Text(interpreted.text.clone()),
            };
        }

        let mut parts = Vec::new();

        if !interpreted.text.is_empty() {
            parts.push(ContentPart::Text {
                text: interpreted.text.clone(),
            });
        }

        for attachment in &interpreted.attachments {
            match attachment {
                Attachment::Image { base64, mime_type } => {
                    parts.push(ContentPart::ImageUrl {
                        image_url: MediaUrl {
                            url: format!("data:{};base64,{}", mime_type, base64),
                        },
                    });
                }
                Attachment::Video { base64, mime_type } => {
                    parts.push(ContentPart::VideoUrl {
                        video_url: MediaUrl {
                            url: format!("data:{};base64,{}", mime_type, base64),
                        },
                    });
                }
            }
        }

        Message::User {
            content: UserContent::Parts(parts),
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
    pub fn new(
        http: reqwest::Client,
        base_url: String,
        api_key: String,
        model: String,
    ) -> Self {
        Self {
            http,
            base_url,
            api_key,
            model,
        }
    }

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

    /// Returns the platform-provided builtin tools (e.g. $web_search).
    /// These are server-side tools executed by the Kimi platform, not locally.
    fn platform_builtins() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            r#type: "builtin_function".to_owned(),
            function: FunctionDefinition {
                name: "$web_search".to_owned(),
                description: None,
                parameters: None,
            },
        }]
    }

    pub fn build_messages(
        &self,
        system_prompt: &str,
        history: &[Message],
        interpreted: &InterpretedMessage,
    ) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(history.len() + 2);
        msgs.push(Message::System {
            content: system_prompt.to_owned(),
        });
        msgs.extend_from_slice(history);
        msgs.push(Message::from_interpreted(interpreted));
        msgs
    }

    /// Builds messages from history only (no new user input). Used in the tool use loop
    /// where user input is already in history.
    pub fn build_messages_from_history(
        &self,
        system_prompt: &str,
        history: &[Message],
    ) -> Vec<Message> {
        let mut msgs = Vec::with_capacity(history.len() + 1);
        msgs.push(Message::System {
            content: system_prompt.to_owned(),
        });
        msgs.extend_from_slice(history);
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
            content: UserContent::Text("Hello".into()),
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Hello");

        let asst = Message::Assistant {
            content: Some("Hi!".into()),
            reasoning_content: None,
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
            name: None,
            content: "result".into(),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_123");
        assert_eq!(json["content"], "result");
    }

    #[test]
    fn test_user_content_text_serialization() {
        let content = UserContent::Text("Hello".into());
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.is_string());
        assert_eq!(json, "Hello");
    }

    #[test]
    fn test_user_content_parts_serialization() {
        let content = UserContent::Parts(vec![
            ContentPart::Text {
                text: "Explain this".into(),
            },
            ContentPart::ImageUrl {
                image_url: MediaUrl {
                    url: "data:image/jpeg;base64,abc123".into(),
                },
            },
        ]);
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.is_array());
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "Explain this");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/jpeg;base64,abc123");
    }

    #[test]
    fn test_multimodal_user_message_serialization() {
        let msg = Message::User {
            content: UserContent::Parts(vec![
                ContentPart::Text {
                    text: "What is this?".into(),
                },
                ContentPart::ImageUrl {
                    image_url: MediaUrl {
                        url: "data:image/png;base64,xyz".into(),
                    },
                },
            ]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert!(json["content"].is_array());
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
                content: UserContent::Text("First".into()),
            },
            Message::Assistant {
                content: Some("Reply".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        ];

        let interpreted = InterpretedMessage {
            text: "Second".into(),
            attachments: vec![],
        };

        let msgs = client.build_messages("System prompt", &history, &interpreted);

        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[0], Message::System { content } if content == "System prompt"));
        assert!(matches!(&msgs[3], Message::User { content: UserContent::Text(t) } if t == "Second"));
    }
}
