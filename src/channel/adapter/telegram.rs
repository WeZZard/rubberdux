use teloxide::prelude::*;
use teloxide::types::{MessageReactionUpdated, ReactionType, Recipient};
use tokio::sync::mpsc;

use crate::channel::interpreter;
use crate::channel::{AgentResponse, UserMessage};

const TELEGRAM_PROMPT: &str = include_str!("telegram_prompt.md");

/// Returns the Telegram channel prompt partial for system prompt composition.
pub fn channel_prompt() -> &'static str {
    TELEGRAM_PROMPT
}

/// Parses model output for `<telegram-message from="assistant" to="user">` tags.
/// Returns the content to send as a Telegram message.
fn extract_reply(text: &str) -> Option<String> {
    let start_tag = "<telegram-message from=\"assistant\" to=\"user\">";
    let end_tag = "</telegram-message>";

    let start = text.find(start_tag)?;
    let content_start = start + start_tag.len();
    let end = text[content_start..].find(end_tag)?;
    Some(text[content_start..content_start + end].to_owned())
}

/// Parses model output for `<telegram-reaction from="assistant"` tags.
/// Returns (emoji, message_id) pairs.
fn extract_reactions(text: &str) -> Vec<(String, i32)> {
    let mut reactions = Vec::new();
    let tag_prefix = "<telegram-reaction from=\"assistant\"";

    let mut search_from = 0;
    while let Some(start) = text[search_from..].find(tag_prefix) {
        let tag_start = search_from + start;
        if let Some(tag_end) = text[tag_start..].find("/>") {
            let tag = &text[tag_start..tag_start + tag_end + 2];

            let emoji = extract_attr(tag, "emoji");
            let message_id = extract_attr(tag, "message-id")
                .and_then(|s| s.parse::<i32>().ok());

            if let (Some(emoji), Some(mid)) = (emoji, message_id) {
                reactions.push((emoji, mid));
            }

            search_from = tag_start + tag_end + 2;
        } else {
            break;
        }
    }
    reactions
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let pattern = format!("{}=\"", name);
    let start = tag.find(&pattern)?;
    let value_start = start + pattern.len();
    let end = tag[value_start..].find('"')?;
    Some(tag[value_start..value_start + end].to_owned())
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

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentResponse>();

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

    match reply_rx.await {
        Ok(response) => {
            // Parse structured model output
            let reply_text = extract_reply(&response.text);
            let reactions = extract_reactions(&response.text);

            // Send reactions
            for (emoji, message_id) in &reactions {
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

            // Send reply message
            if let Some(text) = reply_text {
                let formatted = crate::markdown::telegram::format(&text);
                log::debug!("Raw model reply:\n{}", text);
                log::debug!("Formatted for Telegram:\n{}", formatted);

                let send_result = bot
                    .send_message(msg.chat.id, &formatted)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .await;

                if let Err(e) = send_result {
                    log::warn!("MarkdownV2 send failed ({}), retrying without parse_mode", e);
                    bot.send_message(msg.chat.id, &text).await?;
                }
            } else if reactions.is_empty() {
                // No structured output — fallback: send raw response
                let formatted = crate::markdown::telegram::format(&response.text);
                log::debug!("Raw LLM response (no tags):\n{}", response.text);

                let send_result = bot
                    .send_message(msg.chat.id, &formatted)
                    .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                    .await;

                if let Err(e) = send_result {
                    log::warn!("MarkdownV2 send failed ({}), retrying without parse_mode", e);
                    bot.send_message(msg.chat.id, &response.text).await?;
                }
            }
        }
        Err(_) => {
            log::error!("Agent dropped the reply channel");
            bot.send_message(msg.chat.id, "Sorry, failed to get a response.")
                .await?;
        }
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
