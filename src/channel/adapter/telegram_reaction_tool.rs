use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{MessageId, ReactionType};
use tokio::sync::Mutex;

use crate::provider::moonshot::tool::ToolDefinition;
use crate::tool::{Tool, ToolOutcome};

pub struct TelegramReactionTool {
    bot: Bot,
    chat_id: Arc<Mutex<Option<i64>>>,
}

impl TelegramReactionTool {
    pub fn new(bot: Bot, chat_id: Arc<Mutex<Option<i64>>>) -> Self {
        Self { bot, chat_id }
    }
}

impl Tool for TelegramReactionTool {
    fn name(&self) -> &str {
        "telegram_reaction"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("telegram_reaction.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!("Failed to parse arguments: {}", e),
                        is_error: true,
                    };
                }
            };

            let action = args["action"].as_str().unwrap_or("set");
            let emoji = match args["emoji"].as_str() {
                Some(e) => e.to_string(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: emoji".into(),
                        is_error: true,
                    };
                }
            };
            let message_id = match args["message_id"].as_i64() {
                Some(id) => id as i32,
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: message_id".into(),
                        is_error: true,
                    };
                }
            };

            let chat_id = {
                let guard = self.chat_id.lock().await;
                match *guard {
                    Some(id) => id,
                    None => {
                        return ToolOutcome::Immediate {
                            content: "No active Telegram chat. Send a message first.".into(),
                            is_error: true,
                        };
                    }
                }
            };

            let reactions = if action == "unset" {
                vec![]
            } else {
                vec![ReactionType::Emoji {
                    emoji: emoji.clone(),
                }]
            };

            match self
                .bot
                .set_message_reaction(ChatId(chat_id), MessageId(message_id))
                .reaction(reactions)
                .await
            {
                Ok(_) => ToolOutcome::Immediate {
                    content: format!(
                        "Reaction {} on message {}",
                        if action == "unset" {
                            "removed"
                        } else {
                            &emoji
                        },
                        message_id
                    ),
                    is_error: false,
                },
                Err(e) => ToolOutcome::Immediate {
                    content: format!("Failed to set reaction: {}", e),
                    is_error: true,
                },
            }
        })
    }
}
