use teloxide::prelude::*;
use teloxide::types::{MessageReactionUpdated, ReactionType, Recipient};
use tokio::sync::mpsc;

use super::markup::{self, Document, MessageElement, Node};
use super::parser::{self, Segment};
use crate::channel::interpreter;
use crate::channel::{AgentResponse, UserMessage};

const TELEGRAM_PROMPT: &str = include_str!("TELEGRAM.md");

/// Returns the Telegram channel prompt partial for system prompt composition.
pub fn channel_prompt() -> &'static str {
    TELEGRAM_PROMPT
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    tx: mpsc::Sender<UserMessage>,
) -> Result<(), teloxide::RequestError> {
    let interpreted = match interpreter::interpret(&bot, &msg).await {
        Some(m) => m,
        None => return Ok(()),
    };

    log::info!(
        "Received: {} (attachments: {})",
        interpreted.text,
        interpreted.attachments.len()
    );

    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<AgentResponse>(16);

    let user_message = UserMessage {
        interpreted,
        reply_tx: Some(reply_tx),
    };

    if tx.send(user_message).await.is_err() {
        log::error!("Agent loop channel closed");
        bot.send_message(msg.chat.id, "Sorry, the agent is unavailable.")
            .await?;
        return Ok(());
    }

    let _ = bot
        .send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await;

    while let Some(response) = reply_rx.recv().await {
        // Parse structured model output using the recursive descent parser
        let segments = parser::parse_model_output(&response.text);

        let mut has_reply = false;

        for segment in &segments {
            match segment {
                Segment::TelegramReaction { emoji, message_id } => {
                    let reaction = ReactionType::Emoji {
                        emoji: emoji.clone(),
                    };
                    let result = bot
                        .set_message_reaction(
                            Recipient::Id(msg.chat.id),
                            teloxide::types::MessageId(*message_id),
                        )
                        .reaction(vec![reaction])
                        .await;

                    if let Err(e) = result {
                        log::warn!("Failed to set reaction: {}", e);
                    }
                }
                Segment::TelegramMessage { content } => {
                    has_reply = true;
                    let formatted = super::markdown::format(content);
                    log::debug!("Raw model reply:\n{}", content);
                    log::debug!("Formatted for Telegram:\n{}", formatted);

                    let sent_msg = bot
                        .send_message(msg.chat.id, &formatted)
                        .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                        .await;

                    let sent_msg = match sent_msg {
                        Ok(m) => Some(m),
                        Err(e) => {
                            log::warn!(
                                "MarkdownV2 send failed ({}), retrying without parse_mode",
                                e
                            );
                            bot.send_message(msg.chat.id, content).await.ok()
                        }
                    };

                    // Report bot-sent message ID back to agent loop (final messages only)
                    if response.is_final {
                        if let Some(sent) = sent_msg {
                            let update_text = format!(
                                "__update_assistant_id:{}:{}",
                                sent.id.0, response.history_index
                            );
                            let update_msg = UserMessage {
                                interpreted: crate::channel::interpreter::InterpretedMessage {
                                    text: update_text,
                                    attachments: vec![],
                                },
                                reply_tx: None,
                            };
                            let _ = tx.send(update_msg).await;
                        }
                    }
                }
                Segment::Internal(_) => {
                    // Internal reasoning — not sent to user
                }
            }
        }

        // Fallback: if no telegram-message tags found, send raw text
        if !has_reply
            && segments
                .iter()
                .all(|s| !matches!(s, Segment::TelegramReaction { .. }))
        {
            if !response.text.is_empty() {
                let formatted = super::markdown::format(&response.text);
                log::debug!("Raw LLM response (no tags):\n{}", response.text);

                let sent_msg = bot
                    .send_message(msg.chat.id, &formatted)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .await;

                match sent_msg {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!(
                            "MarkdownV2 send failed ({}), retrying without parse_mode",
                            e
                        );
                        let _ = bot.send_message(msg.chat.id, &response.text).await;
                    }
                }
            }
        }

        if response.is_final {
            break;
        }

        // Show typing indicator between intermediate messages
        let _ = bot
            .send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
            .await;
    }

    Ok(())
}

async fn handle_reaction(
    reaction: MessageReactionUpdated,
    tx: mpsc::Sender<UserMessage>,
) -> Result<(), teloxide::RequestError> {
    let interpreted_messages = interpreter::interpret_reaction(&reaction);

    for interpreted in interpreted_messages {
        log::info!("Reaction event: {}", interpreted.text);

        let user_message = UserMessage {
            interpreted,
            reply_tx: None,
        };

        if tx.send(user_message).await.is_err() {
            log::error!("Agent loop channel closed (reaction)");
        }
    }

    Ok(())
}

pub async fn run(bot: Bot, tx: mpsc::Sender<UserMessage>) {
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_message_reaction_updated().endpoint(handle_reaction));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![tx])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/// Injects a Telegram message ID into an assistant message's content.
/// If the content already has a `<telegram-message from="assistant" to="user">` tag,
/// the `id` attribute is inserted into the existing tag.
/// Otherwise, the content is wrapped in a new tag.
pub fn inject_assistant_message_id(text: &mut String, msg_id: i32) {
    let mut doc = markup::parse(text);

    let mut found = false;
    for node in &mut doc.nodes {
        if let Node::Message(el) = node
            && el.from == "assistant"
            && el.to == "user"
        {
            if el.id.is_some() {
                return; // Already has id — idempotent
            }
            el.id = Some(msg_id.to_string());
            found = true;
            break;
        }
    }

    if found {
        *text = markup::serialize(&doc);
    } else {
        // No matching tag — wrap entire content
        let wrapped = Document {
            nodes: vec![Node::Message(MessageElement {
                from: "assistant".into(),
                to: "user".into(),
                id: Some(msg_id.to_string()),
                date: None,
                content: text.clone(),
            })],
        };
        *text = markup::serialize(&wrapped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::moonshot::{Message, UserContent};

    #[test]
    fn test_inject_id_into_existing_telegram_tag() {
        let mut text =
            "<telegram-message from=\"assistant\" to=\"user\">Hello!</telegram-message>".to_owned();
        inject_assistant_message_id(&mut text, 73);

        assert_eq!(
            text,
            "<telegram-message from=\"assistant\" to=\"user\" id=\"73\">Hello!</telegram-message>"
        );
        assert_eq!(text.matches("<telegram-message").count(), 1);
        assert_eq!(text.matches("</telegram-message>").count(), 1);
    }

    #[test]
    fn test_inject_id_wraps_plain_text() {
        let mut text = "Hello!".to_owned();
        inject_assistant_message_id(&mut text, 73);

        assert_eq!(
            text,
            "<telegram-message from=\"assistant\" to=\"user\" id=\"73\">Hello!</telegram-message>"
        );
    }

    #[test]
    fn test_inject_id_does_not_double_nest() {
        let mut text =
            "<telegram-message from=\"assistant\" to=\"user\">Some content</telegram-message>"
                .to_owned();
        inject_assistant_message_id(&mut text, 42);

        assert_eq!(text.matches("<telegram-message").count(), 1);
        assert_eq!(text.matches("</telegram-message>").count(), 1);
        assert!(text.contains("id=\"42\""));
    }

    #[test]
    fn test_inject_id_idempotent_when_called_twice() {
        let mut text =
            "<telegram-message from=\"assistant\" to=\"user\">Hello!</telegram-message>".to_owned();
        inject_assistant_message_id(&mut text, 106);
        inject_assistant_message_id(&mut text, 106);

        assert_eq!(
            text.matches("<telegram-message").count(),
            1,
            "Double injection created nested tags: {}",
            text
        );
        assert_eq!(text.matches("</telegram-message>").count(), 1);
        assert!(text.contains("id=\"106\""));
    }

    #[test]
    fn test_inject_id_preserves_content_with_inner_tags() {
        let mut text = "<telegram-message from=\"assistant\" to=\"user\">Here is a <b>bold</b> word</telegram-message>".to_owned();
        inject_assistant_message_id(&mut text, 99);

        assert!(text.contains("id=\"99\""));
        assert!(text.contains("<b>bold</b>"));
        assert_eq!(text.matches("<telegram-message").count(), 1);
    }

    #[test]
    fn test_inject_id_into_history_modifies_in_place() {
        let mut history = vec![
            Message::User {
                content: UserContent::Text("Hello".into()),
            },
            Message::Assistant {
                content: Some(
                    "<telegram-message from=\"assistant\" to=\"user\">Hi there!</telegram-message>"
                        .into(),
                ),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        ];

        if let Some(Message::Assistant { content, .. }) = history.get_mut(1) {
            if let Some(text) = content {
                inject_assistant_message_id(text, 55);
            }
        }

        let asst_content = history[1].content_text();
        assert!(asst_content.contains("id=\"55\""));
        assert_eq!(asst_content.matches("<telegram-message").count(), 1);

        assert_eq!(history.len(), 2);
    }
}
