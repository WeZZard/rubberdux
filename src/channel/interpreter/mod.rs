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

/// Interprets a Telegram message into a unified format for the agent loop.
pub async fn interpret(bot: &Bot, msg: &Message) -> Option<InterpretedMessage> {
    // Text message
    if let Some(text) = msg.text() {
        return Some(InterpretedMessage {
            text: text.to_owned(),
            attachments: vec![],
        });
    }

    // Photo
    if let Some(photos) = msg.photo() {
        return Some(media::interpret_photo(bot, photos, msg.caption()).await);
    }

    // Video
    if let Some(video) = msg.video() {
        return Some(media::interpret_video(bot, video, msg.caption()).await);
    }

    // Animation (GIF)
    if let Some(animation) = msg.animation() {
        return Some(media::interpret_animation(bot, animation, msg.caption()).await);
    }

    // Document
    if let Some(document) = msg.document() {
        return Some(media::interpret_document(bot, document, msg.caption()).await);
    }

    // Voice
    if let Some(voice_msg) = msg.voice() {
        return Some(voice::interpret_voice(bot, voice_msg).await);
    }

    // Audio
    if let Some(audio) = msg.audio() {
        return Some(voice::interpret_audio(bot, audio).await);
    }

    // Video note
    if let Some(video_note) = msg.video_note() {
        return Some(voice::interpret_video_note(bot, video_note).await);
    }

    // Location
    if let Some(location) = msg.location() {
        return Some(structured::interpret_location(location));
    }

    // Contact
    if let Some(contact) = msg.contact() {
        return Some(structured::interpret_contact(contact));
    }

    // Venue
    if let Some(venue) = msg.venue() {
        return Some(structured::interpret_venue(venue));
    }

    // Poll
    if let Some(poll) = msg.poll() {
        return Some(structured::interpret_poll(poll));
    }

    // Dice
    if let Some(dice) = msg.dice() {
        return Some(structured::interpret_dice(dice));
    }

    // Sticker
    if let Some(sticker) = msg.sticker() {
        return Some(structured::interpret_sticker(sticker));
    }

    None
}
