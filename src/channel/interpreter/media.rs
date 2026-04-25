use base64::Engine;
use teloxide::prelude::*;
use teloxide::types::{Animation, Document, PhotoSize, Video};

use super::{Attachment, InterpretedMessage};

async fn download_file(bot: &Bot, file_id: &str) -> Option<Vec<u8>> {
    let file = bot.get_file(file_id).await.ok()?;
    let mut buf = Vec::new();
    teloxide::net::Download::download_file(bot, &file.path, &mut buf)
        .await
        .ok()?;
    Some(buf)
}

pub async fn interpret_photo(
    bot: &Bot,
    photos: &[PhotoSize],
    caption: Option<&str>,
) -> InterpretedMessage {
    let largest = photos.iter().max_by_key(|p| p.width * p.height);

    let attachments = match largest {
        Some(photo) => match download_file(bot, &photo.file.id).await {
            Some(data) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                vec![Attachment::Image {
                    base64: b64,
                    mime_type: "image/jpeg".into(),
                }]
            }
            None => {
                log::error!("Failed to download photo");
                vec![]
            }
        },
        None => vec![],
    };

    InterpretedMessage {
        text: caption.unwrap_or("").to_owned(),
        attachments,
    }
}

pub async fn interpret_video(
    bot: &Bot,
    video: &Video,
    caption: Option<&str>,
) -> InterpretedMessage {
    let mime = video
        .mime_type
        .as_ref()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "video/mp4".into());

    let attachments = match download_file(bot, &video.file.id).await {
        Some(data) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            vec![Attachment::Video {
                base64: b64,
                mime_type: mime,
            }]
        }
        None => {
            log::error!("Failed to download video");
            vec![]
        }
    };

    InterpretedMessage {
        text: caption.unwrap_or("").to_owned(),
        attachments,
    }
}

pub async fn interpret_animation(
    bot: &Bot,
    animation: &Animation,
    caption: Option<&str>,
) -> InterpretedMessage {
    let attachments = match download_file(bot, &animation.file.id).await {
        Some(data) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            vec![Attachment::Video {
                base64: b64,
                mime_type: "video/mp4".into(),
            }]
        }
        None => {
            log::error!("Failed to download animation");
            vec![]
        }
    };

    InterpretedMessage {
        text: caption.unwrap_or("").to_owned(),
        attachments,
    }
}

pub async fn interpret_document(
    bot: &Bot,
    document: &Document,
    caption: Option<&str>,
) -> InterpretedMessage {
    let mime = document
        .mime_type
        .as_ref()
        .map(|m| m.to_string())
        .unwrap_or_default();

    let filename = document.file_name.as_deref().unwrap_or("unknown");

    // For image/video documents, treat as media
    if mime.starts_with("image/")
        && let Some(data) = download_file(bot, &document.file.id).await
    {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        return InterpretedMessage {
            text: caption.unwrap_or("").to_owned(),
            attachments: vec![Attachment::Image {
                base64: b64,
                mime_type: mime,
            }],
        };
    }

    if mime.starts_with("video/")
        && let Some(data) = download_file(bot, &document.file.id).await
    {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        return InterpretedMessage {
            text: caption.unwrap_or("").to_owned(),
            attachments: vec![Attachment::Video {
                base64: b64,
                mime_type: mime,
            }],
        };
    }

    // For other documents, note the filename
    let caption_text = caption.unwrap_or("");
    let text = if caption_text.is_empty() {
        format!("<document filename=\"{}\" />", filename)
    } else {
        format!("{}\n<document filename=\"{}\" />", caption_text, filename)
    };

    InterpretedMessage {
        text,
        attachments: vec![],
    }
}
