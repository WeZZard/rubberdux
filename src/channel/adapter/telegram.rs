use teloxide::prelude::*;
use teloxide::types::{MessageReactionUpdated, ReactionType, Recipient};
use tokio::sync::mpsc;

use super::parser::{self, Segment};
use crate::channel::interpreter;
use crate::channel::{AgentResponse, UserMessage};

const TELEGRAM_PROMPT: &str = include_str!("telegram_prompt.md");

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
                    let formatted = crate::markdown::telegram::format(content);
                    log::debug!("Raw model reply:\n{}", content);
                    log::debug!("Formatted for Telegram:\n{}", formatted);

                    let sent_msg = bot
                        .send_message(msg.chat.id, &formatted)
                        .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                        .await;

                    let sent_msg = match sent_msg {
                        Ok(m) => Some(m),
                        Err(e) => {
                            log::warn!("MarkdownV2 send failed ({}), retrying without parse_mode", e);
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
        if !has_reply && segments.iter().all(|s| !matches!(s, Segment::TelegramReaction { .. })) {
            if !response.text.is_empty() {
                let formatted = crate::markdown::telegram::format(&response.text);
                log::debug!("Raw LLM response (no tags):\n{}", response.text);

                let sent_msg = bot
                    .send_message(msg.chat.id, &formatted)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .await;

                match sent_msg {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("MarkdownV2 send failed ({}), retrying without parse_mode", e);
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
