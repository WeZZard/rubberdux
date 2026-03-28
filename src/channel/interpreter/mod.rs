pub mod media;
pub mod structured;
pub mod voice;

use teloxide::prelude::*;

#[derive(Debug, Clone)]
pub struct InterpretedMessage {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone)]
pub enum Attachment {
    Image { base64: String, mime_type: String },
    Video { base64: String, mime_type: String },
}

/// Wraps interpreted content in a `<telegram-message>` tag with metadata.
fn wrap_telegram_message(msg: &Message, inner: InterpretedMessage) -> InterpretedMessage {
    let id = msg.id.0;
    let date = msg.date.to_string();
    InterpretedMessage {
        text: format!(
            "<telegram-message from=\"user\" to=\"assistant\" id=\"{}\" date=\"{}\">{}</telegram-message>",
            id, date, inner.text
        ),
        attachments: inner.attachments,
    }
}

/// Interprets a Telegram message into a unified format for the agent loop.
pub async fn interpret(bot: &Bot, msg: &Message) -> Option<InterpretedMessage> {
    let inner = interpret_content(bot, msg).await?;
    Some(wrap_telegram_message(msg, inner))
}

async fn interpret_content(bot: &Bot, msg: &Message) -> Option<InterpretedMessage> {
    if let Some(text) = msg.text() {
        return Some(InterpretedMessage {
            text: text.to_owned(),
            attachments: vec![],
        });
    }

    if let Some(photos) = msg.photo() {
        return Some(media::interpret_photo(bot, photos, msg.caption()).await);
    }

    if let Some(video) = msg.video() {
        return Some(media::interpret_video(bot, video, msg.caption()).await);
    }

    if let Some(animation) = msg.animation() {
        return Some(media::interpret_animation(bot, animation, msg.caption()).await);
    }

    if let Some(document) = msg.document() {
        return Some(media::interpret_document(bot, document, msg.caption()).await);
    }

    if let Some(voice_msg) = msg.voice() {
        return Some(voice::interpret_voice(bot, voice_msg).await);
    }

    if let Some(audio) = msg.audio() {
        return Some(voice::interpret_audio(bot, audio).await);
    }

    if let Some(video_note) = msg.video_note() {
        return Some(voice::interpret_video_note(bot, video_note).await);
    }

    if let Some(location) = msg.location() {
        return Some(structured::interpret_location(location));
    }

    if let Some(contact) = msg.contact() {
        return Some(structured::interpret_contact(contact));
    }

    if let Some(venue) = msg.venue() {
        return Some(structured::interpret_venue(venue));
    }

    if let Some(poll) = msg.poll() {
        return Some(structured::interpret_poll(poll));
    }

    if let Some(dice) = msg.dice() {
        return Some(structured::interpret_dice(dice));
    }

    if let Some(sticker) = msg.sticker() {
        return Some(structured::interpret_sticker(sticker));
    }

    None
}

/// Interprets a Telegram reaction event into an InterpretedMessage.
pub fn interpret_reaction(
    reaction: &teloxide::types::MessageReactionUpdated,
) -> Vec<InterpretedMessage> {
    let message_id = reaction.message_id.0;
    let date = reaction.date.to_string();
    let mut results = Vec::new();

    // Determine added reactions (in new but not in old)
    for new_r in &reaction.new_reaction {
        let is_new = !reaction.old_reaction.iter().any(|old| old == new_r);
        if is_new
            && let Some(emoji) = new_r.emoji() {
                results.push(InterpretedMessage {
                    text: format!(
                        "<telegram-reaction from=\"user\" action=\"add\" emoji=\"{}\" message-id=\"{}\" date=\"{}\" />",
                        emoji, message_id, date
                    ),
                    attachments: vec![],
                });
            }
    }

    // Determine removed reactions (in old but not in new)
    for old_r in &reaction.old_reaction {
        let is_removed = !reaction.new_reaction.iter().any(|new| new == old_r);
        if is_removed
            && let Some(emoji) = old_r.emoji() {
                results.push(InterpretedMessage {
                    text: format!(
                        "<telegram-reaction from=\"user\" action=\"remove\" emoji=\"{}\" message-id=\"{}\" date=\"{}\" />",
                        emoji, message_id, date
                    ),
                    attachments: vec![],
                });
            }
    }

    results
}
