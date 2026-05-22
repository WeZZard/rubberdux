use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use teloxide::prelude::*;
use teloxide::types::{MessageReactionUpdated, ReactionType};
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use super::markup::{self, Document, MessageElement, Node};
use super::parser::{self, Segment};
use crate::agent::entry::{Entry, EntryOrigin};
use crate::agent::runtime::port::{EntryNotification, InputPort, InternalMutation, LoopEvent};
use crate::channel::interpreter;
use crate::channel::processor::ChannelProcessor;
use crate::error::Error;
use crate::provider::moonshot::{Message, UserContent};

const TELEGRAM_PROMPT: &str = include_str!("TELEGRAM.md");

/// Default reaction emojis from teloxide's ReactionType::Emoji documentation.
/// Used as fallback when get_chat returns None for available_reactions.
pub const DEFAULT_REACTIONS: &[&str] = &[
    "👍",
    "👎",
    "❤",
    "🔥",
    "🥰",
    "👏",
    "😁",
    "🤔",
    "🤯",
    "😱",
    "🤬",
    "😢",
    "🎉",
    "🤩",
    "🤮",
    "💩",
    "🙏",
    "👌",
    "🕊",
    "🤡",
    "🥱",
    "🥴",
    "😍",
    "🐳",
    "❤\u{200d}🔥",
    "🌚",
    "🌭",
    "💯",
    "🤣",
    "⚡",
    "🍌",
    "🏆",
    "💔",
    "🤨",
    "😐",
    "🍓",
    "🍾",
    "💋",
    "🖕",
    "😈",
    "😴",
    "😭",
    "🤓",
    "👻",
    "👨\u{200d}💻",
    "👀",
    "🎃",
    "🙈",
    "😇",
    "😨",
    "🤝",
    "✍",
    "🤗",
    "🫡",
    "🎅",
    "🎄",
    "☃",
    "💅",
    "🤪",
    "🗿",
    "🆒",
    "💘",
    "🙉",
    "🦄",
    "😘",
    "💊",
    "🙊",
    "😎",
    "👾",
    "🤷\u{200d}♂",
    "🤷",
    "🤷\u{200d}♀",
    "😡",
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

/// Run the Telegram adapter.
pub async fn run(
    bot: Bot,
    input_port: InputPort,
    entry_rx: broadcast::Receiver<EntryNotification>,
) {
    let reactions_fetched = Arc::new(AtomicBool::new(false));
    let entry_cache: Arc<Mutex<HashMap<usize, Entry>>> = Arc::new(Mutex::new(HashMap::new()));

    // Spawn broadcast listener
    let cache_for_broadcast = entry_cache.clone();
    tokio::spawn(broadcast_listener(
        entry_rx,
        cache_for_broadcast,
    ));

    // Run teloxide dispatcher
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_message_reaction_updated().endpoint(handle_reaction));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![input_port, reactions_fetched, entry_cache])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn broadcast_listener(
    mut entry_rx: broadcast::Receiver<EntryNotification>,
    cache: Arc<Mutex<HashMap<usize, Entry>>>,
) {
    loop {
        match entry_rx.recv().await {
            Ok(notification) => {
                let mut c = cache.lock().await;
                c.insert(notification.entry.id, notification.entry.clone());
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("Telegram broadcast listener lagged by {} messages", n);
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
fn find_telegram_context(
    cache: &HashMap<usize, Entry>,
    entry: &Entry,
) -> (Option<i64>, Option<i32>) {
    // Walk parent chain to find the root user entry from Telegram
    let mut current = entry;
    loop {
        match current.parent_id {
            Some(parent_id) => {
                if let Some(parent) = cache.get(&parent_id) {
                    current = parent;
                } else {
                    return (None, None);
                }
            }
            None => break,
        }
    }

    // current is now the root entry
    if let EntryOrigin::User { ref channel } = current.origin {
        if channel == "telegram" {
            if let Some(ref meta) = current.channel_metadata {
                let chat_id = meta.get("telegram_chat_id").and_then(|v| v.as_i64());
                let msg_id = meta
                    .get("telegram_message_id")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
                return (chat_id, msg_id);
            }
        }
    }

    (None, None)
}

async fn handle_message(
    bot: Bot,
    msg: teloxide::types::Message,
    input_port: InputPort,
    reactions_fetched: Arc<AtomicBool>,
    _entry_cache: Arc<Mutex<HashMap<usize, Entry>>>,
) -> Result<(), teloxide::RequestError> {
    // Fetch available reactions on first message
    if !reactions_fetched.swap(true, Ordering::Relaxed) {
        fetch_and_send_reactions(&bot, msg.chat.id, &input_port).await;
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

    let message = Message::from_interpreted(&interpreted);

    let channel_metadata = Some(serde_json::json!({
        "telegram_message_id": msg.id.0,
        "telegram_chat_id": msg.chat.id.0,
    }));

    if input_port
        .send(LoopEvent::UserMessage {
            message,
            origin: EntryOrigin::User {
                channel: "telegram".into(),
            },
            channel_metadata,
        })
        .await
        .is_err()
    {
        log::error!("AgentLoop input closed");
        bot.send_message(msg.chat.id, "Sorry, the agent is unavailable.")
            .await?;
        return Ok(());
    }

    let _ = bot
        .send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await;

    Ok(())
}

async fn handle_reaction(
    reaction: MessageReactionUpdated,
    input_port: InputPort,
) -> Result<(), teloxide::RequestError> {
    let interpreted_messages = interpreter::interpret_reaction(&reaction);

    let channel_metadata = Some(serde_json::json!({
        "telegram_chat_id": reaction.chat.id.0,
        "telegram_message_id": reaction.message_id.0,
    }));

    for interpreted in interpreted_messages {
        log::info!("Reaction event: {}", interpreted.text);

        let message = Message::User {
            content: UserContent::Text(interpreted.text),
        };

        if input_port
            .send(LoopEvent::UserMessage {
                message,
                origin: EntryOrigin::User {
                    channel: "telegram".into(),
                },
                channel_metadata: channel_metadata.clone(),
            })
            .await
            .is_err()
        {
            log::error!("AgentLoop input closed (reaction)");
        }
    }

    Ok(())
}

async fn fetch_and_send_reactions(bot: &Bot, chat_id: ChatId, input_port: &InputPort) {
    let emojis: Vec<String> = match bot.get_chat(chat_id).await {
        Ok(chat) => match chat.available_reactions {
            Some(reactions) => reactions
                .into_iter()
                .filter_map(|r| match r {
                    ReactionType::Emoji { emoji } => Some(emoji),
                    _ => None,
                })
                .collect(),
            None => DEFAULT_REACTIONS.iter().map(|s| s.to_string()).collect(),
        },
        Err(e) => {
            log::warn!("Failed to fetch chat info for reactions: {}", e);
            DEFAULT_REACTIONS.iter().map(|s| s.to_string()).collect()
        }
    };

    let section = format_reaction_section(&emojis);
    log::info!(
        "Fetched {} available reactions for chat {}",
        emojis.len(),
        chat_id
    );

    let _ = input_port
        .send(LoopEvent::Internal(InternalMutation::UpdateSystemPrompt {
            content: section,
        }))
        .await;
}

pub struct TelegramChannelProcessor {
    bot: Bot,
}

impl TelegramChannelProcessor {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }
}

impl ChannelProcessor for TelegramChannelProcessor {
    fn channel_name(&self) -> &str {
        "telegram"
    }

    fn process_outbound<'a>(
        &'a self,
        entry: &'a mut Entry,
        channel_metadata: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>> {
        Box::pin(async move {
            // Only process assistant entries
            if entry.origin != EntryOrigin::Assistant {
                return Ok(());
            }

            // Extract telegram_chat_id; if missing, this is not a Telegram conversation
            let chat_id = match channel_metadata.get("telegram_chat_id").and_then(|v| v.as_i64()) {
                Some(id) => id,
                None => return Ok(()),
            };

            // Get the content text from the entry
            let text = entry.message.content_text().to_owned();
            if text.is_empty() {
                return Ok(());
            }

            // Parse content to find Telegram message segments
            let segments = parser::parse_model_output(&text);

            let reply_to_msg_id = channel_metadata
                .get("telegram_message_id")
                .and_then(|v| v.as_i64())
                .map(|v| v as i32);

            for segment in &segments {
                if let Segment::TelegramMessage { content } = segment {
                    let formatted = super::markdown::format(content);

                    let mut req = self
                        .bot
                        .send_message(ChatId(chat_id), &formatted)
                        .parse_mode(teloxide::types::ParseMode::MarkdownV2);
                    if let Some(reply_id) = reply_to_msg_id {
                        req = req.reply_parameters(
                            teloxide::types::ReplyParameters::new(
                                teloxide::types::MessageId(reply_id),
                            ),
                        );
                    }
                    let sent_msg = req.await;

                    let sent_msg = match sent_msg {
                        Ok(m) => Some(m),
                        Err(e) => {
                            log::warn!("MarkdownV2 send failed ({}), retrying plain", e);
                            let mut fallback = self.bot.send_message(ChatId(chat_id), content);
                            if let Some(reply_id) = reply_to_msg_id {
                                fallback = fallback.reply_parameters(
                                    teloxide::types::ReplyParameters::new(
                                        teloxide::types::MessageId(reply_id),
                                    ),
                                );
                            }
                            fallback.await.ok()
                        }
                    };

                    // Inject Telegram message ID into the entry content
                    if let Some(sent) = sent_msg {
                        let msg_id = sent.id.0;
                        if let Message::Assistant {
                            content: Some(text),
                            ..
                        } = &mut entry.message
                        {
                            inject_assistant_message_id(text, msg_id);
                        }
                    }
                }
            }

            Ok(())
        })
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

    #[test]
    fn test_find_telegram_context_simple_chain() {
        let mut cache = HashMap::new();
        cache.insert(
            0,
            Entry {
                id: 0,
                parent_id: None,
                message: Message::User {
                    content: UserContent::Text("hi".into()),
                },
                origin: EntryOrigin::User {
                    channel: "telegram".into(),
                },
                channel_metadata: Some(serde_json::json!({
                    "telegram_chat_id": 12345_i64,
                    "telegram_message_id": 99,
                })),
            },
        );
        cache.insert(
            1,
            Entry {
                id: 1,
                parent_id: Some(0),
                message: Message::Assistant {
                    content: Some("hello".into()),
                    reasoning_content: None,
                    tool_calls: None,
                    partial: None,
                },
                origin: EntryOrigin::Assistant,
                channel_metadata: None,
            },
        );

        let (chat_id, msg_id) = find_telegram_context(&cache, cache.get(&1).unwrap());
        assert_eq!(chat_id, Some(12345));
        assert_eq!(msg_id, Some(99));
    }

    #[test]
    fn test_find_telegram_context_no_parent() {
        let cache = HashMap::new();
        let entry = Entry {
            id: 5,
            parent_id: Some(999),
            message: Message::Assistant {
                content: Some("lost".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
            origin: EntryOrigin::Assistant,
            channel_metadata: None,
        };
        let (chat_id, msg_id) = find_telegram_context(&cache, &entry);
        assert_eq!(chat_id, None);
        assert_eq!(msg_id, None);
    }

    #[test]
    fn test_find_telegram_context_non_telegram_origin() {
        let mut cache = HashMap::new();
        cache.insert(
            0,
            Entry {
                id: 0,
                parent_id: None,
                message: Message::User {
                    content: UserContent::Text("hi".into()),
                },
                origin: EntryOrigin::User {
                    channel: "gateway".into(),
                },
                channel_metadata: None,
            },
        );
        cache.insert(
            1,
            Entry {
                id: 1,
                parent_id: Some(0),
                message: Message::Assistant {
                    content: Some("hello".into()),
                    reasoning_content: None,
                    tool_calls: None,
                    partial: None,
                },
                origin: EntryOrigin::Assistant,
                channel_metadata: None,
            },
        );

        let (chat_id, _) = find_telegram_context(&cache, cache.get(&1).unwrap());
        assert_eq!(chat_id, None);
    }
}
