use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use teloxide::prelude::*;
use teloxide::types::{MessageReactionUpdated, ReactionType, Recipient};
use tokio::sync::mpsc;

use super::markup::{self, Document, MessageElement, Node};
use super::parser::{self, Segment};
use crate::channel::interpreter;
use crate::channel::{AgentResponse, ChannelEvent, InternalEvent};

const TELEGRAM_PROMPT: &str = include_str!("TELEGRAM.md");

/// Default reaction emojis from teloxide's ReactionType::Emoji documentation.
/// Used as fallback when get_chat returns None for available_reactions.
pub const DEFAULT_REACTIONS: &[&str] = &[
    "👍", "👎", "❤", "🔥", "🥰", "👏", "😁", "🤔", "🤯", "😱", "🤬", "😢",
    "🎉", "🤩", "🤮", "💩", "🙏", "👌", "🕊", "🤡", "🥱", "🥴", "😍", "🐳",
    "❤\u{200d}🔥", "🌚", "🌭", "💯", "🤣", "⚡", "🍌", "🏆", "💔", "🤨",
    "😐", "🍓", "🍾", "💋", "🖕", "😈", "😴", "😭", "🤓", "👻", "👨\u{200d}💻",
    "👀", "🎃", "🙈", "😇", "😨", "🤝", "✍", "🤗", "🫡", "🎅", "🎄", "☃",
    "💅", "🤪", "🗿", "🆒", "💘", "🙉", "🦄", "😘", "💊", "🙊", "😎", "👾",
    "🤷\u{200d}♂", "🤷", "🤷\u{200d}♀", "😡",
];

/// Returns the Telegram channel prompt WITHOUT reactions section.
/// Reactions are injected dynamically via UpdateAvailableReactions events.
pub fn channel_prompt() -> &'static str {
    TELEGRAM_PROMPT
}

/// Formats the reaction section for the system prompt.
pub fn format_reaction_section(emojis: &[String]) -> String {
    let emoji_list = emojis.join(" ");
    format!(
        "## Reactions\n\n\
         Available reaction emojis: {}\n\n\
         Guidelines:\n\
         - Use reactions sparingly to acknowledge messages or express genuine sentiment.\n\
         - Do not react to every message. Reserve reactions for moments where they add warmth or clarity.\n\
         - Prefer simple, universally understood emojis (👍 ❤ 🔥 🎉 😁) over niche ones.\n\
         - Never use reactions as a substitute for a text response when the user expects information.\n\
         - Avoid 🖕 and other potentially offensive emojis unless the user explicitly sets a casual tone.\n\
         - Do not use emojis outside this list — they will be rejected by Telegram.",
        emoji_list
    )
}

/// Fetches available reactions for a chat and sends an UpdateAvailableReactions event.
async fn fetch_and_send_reactions(bot: &Bot, chat_id: ChatId, tx: &mpsc::Sender<ChannelEvent>) {
    match bot.get_chat(chat_id).await {
        Ok(chat) => {
            let emojis: Vec<String> = match chat.available_reactions {
                Some(reactions) => reactions
                    .into_iter()
                    .filter_map(|r| match r {
                        ReactionType::Emoji { emoji } => Some(emoji),
                        _ => None,
                    })
                    .collect(),
                None => {
                    // None means all emoji reactions are allowed — use defaults
                    DEFAULT_REACTIONS.iter().map(|s| s.to_string()).collect()
                }
            };
            let section = format_reaction_section(&emojis);
            log::info!("Fetched {} available reactions for chat {}", emojis.len(), chat_id);
            let _ = tx
                .send(ChannelEvent::InternalEvent(
                    InternalEvent::UpdateAvailableReactions {
                        reaction_section: section,
                    },
                ))
                .await;
        }
        Err(e) => {
            log::warn!("Failed to fetch chat info for reactions: {}", e);
            // Fall back to defaults
            let emojis: Vec<String> = DEFAULT_REACTIONS.iter().map(|s| s.to_string()).collect();
            let section = format_reaction_section(&emojis);
            let _ = tx
                .send(ChannelEvent::InternalEvent(
                    InternalEvent::UpdateAvailableReactions {
                        reaction_section: section,
                    },
                ))
                .await;
        }
    }
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    tx: mpsc::Sender<ChannelEvent>,
    reactions_fetched: Arc<AtomicBool>,
) -> Result<(), teloxide::RequestError> {
    // Fetch available reactions on first message
    if !reactions_fetched.swap(true, Ordering::Relaxed) {
        fetch_and_send_reactions(&bot, msg.chat.id, &tx).await;
    }

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

    let event = ChannelEvent::UserInput {
        interpreted,
        reply_tx: Some(reply_tx),
        telegram_message_id: Some(msg.id.0),
    };

    if tx.send(event).await.is_err() {
        log::error!("Agent loop channel closed");
        bot.send_message(msg.chat.id, "Sorry, the agent is unavailable.")
            .await?;
        return Ok(());
    }

    let _ = bot
        .send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await;

    // Spawn reply handling so this function returns immediately,
    // freeing the Telegram dispatcher to process new messages.
    let chat_id = msg.chat.id;
    tokio::spawn(async move {
        while let Some(response) = reply_rx.recv().await {
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
                                Recipient::Id(chat_id),
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

                        let mut req = bot
                            .send_message(chat_id, &formatted)
                            .parse_mode(teloxide::types::ParseMode::MarkdownV2);
                        if let Some(reply_id) = response.reply_to_message_id {
                            req = req.reply_parameters(teloxide::types::ReplyParameters::new(teloxide::types::MessageId(reply_id)));
                        }
                        let sent_msg = req.await;

                        let sent_msg = match sent_msg {
                            Ok(m) => Some(m),
                            Err(e) => {
                                log::warn!(
                                    "MarkdownV2 send failed ({}), retrying without parse_mode",
                                    e
                                );
                                let mut fallback_req = bot.send_message(chat_id, content);
                                if let Some(reply_id) = response.reply_to_message_id {
                                    fallback_req = fallback_req.reply_parameters(teloxide::types::ReplyParameters::new(teloxide::types::MessageId(reply_id)));
                                }
                                fallback_req.await.ok()
                            }
                        };

                        if let Some(sent) = sent_msg {
                            let _ = tx
                                .send(ChannelEvent::InternalEvent(
                                    InternalEvent::UpdateAssistantMessageId {
                                        entry_id: response.entry_id,
                                        message_id: sent.id.0,
                                    },
                                ))
                                .await;
                        }
                    }
                    Segment::Internal(_) => {}
                }
            }

            if !has_reply
                && segments
                    .iter()
                    .all(|s| !matches!(s, Segment::TelegramReaction { .. }))
            {
                if !response.text.is_empty() {
                    let formatted = super::markdown::format(&response.text);
                    log::debug!("Raw LLM response (no tags):\n{}", response.text);

                    let mut req = bot
                        .send_message(chat_id, &formatted)
                        .parse_mode(teloxide::types::ParseMode::MarkdownV2);
                    if let Some(reply_id) = response.reply_to_message_id {
                        req = req.reply_parameters(teloxide::types::ReplyParameters::new(teloxide::types::MessageId(reply_id)));
                    }
                    let sent_msg = req.await;

                    match sent_msg {
                        Ok(_) => {}
                        Err(e) => {
                            log::warn!(
                                "MarkdownV2 send failed ({}), retrying without parse_mode",
                                e
                            );
                            let mut fallback_req = bot.send_message(chat_id, &response.text);
                            if let Some(reply_id) = response.reply_to_message_id {
                                fallback_req = fallback_req.reply_parameters(teloxide::types::ReplyParameters::new(teloxide::types::MessageId(reply_id)));
                            }
                            let _ = fallback_req.await;
                        }
                    }
                }
            }

            if response.is_final {
                break;
            }

            let _ = bot
                .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                .await;
        }
    });

    Ok(())
}

async fn handle_reaction(
    reaction: MessageReactionUpdated,
    tx: mpsc::Sender<ChannelEvent>,
) -> Result<(), teloxide::RequestError> {
    let interpreted_messages = interpreter::interpret_reaction(&reaction);

    for interpreted in interpreted_messages {
        log::info!("Reaction event: {}", interpreted.text);

        let event = ChannelEvent::UserInput {
            interpreted,
            reply_tx: None,
            telegram_message_id: None,
        };

        if tx.send(event).await.is_err() {
            log::error!("Agent loop channel closed (reaction)");
        }
    }

    Ok(())
}

pub async fn run(bot: Bot, tx: mpsc::Sender<ChannelEvent>) {
    let reactions_fetched = Arc::new(AtomicBool::new(false));

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_message_reaction_updated().endpoint(handle_reaction));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![tx, reactions_fetched])
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
