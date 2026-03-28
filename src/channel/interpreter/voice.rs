use teloxide::prelude::*;
use teloxide::types::{Audio, VideoNote, Voice};

use super::InterpretedMessage;

const DEFAULT_WHISPER_LANGUAGE: &str = "auto";

async fn download_to_temp(bot: &Bot, file_id: &str, suffix: &str) -> Option<tempfile::NamedTempFile> {
    let file = bot.get_file(file_id).await.ok()?;
    let temp = tempfile::Builder::new()
        .suffix(suffix)
        .tempfile()
        .ok()?;

    {
        let mut dst = tokio::fs::File::create(temp.path()).await.ok()?;
        teloxide::net::Download::download_file(bot, &file.path, &mut dst)
            .await
            .ok()?;
        // dst is dropped here, flushing the write
    }

    let metadata = std::fs::metadata(temp.path()).ok()?;
    log::debug!(
        "Downloaded voice file to {:?} ({} bytes)",
        temp.path(),
        metadata.len()
    );

    if metadata.len() == 0 {
        log::error!("Downloaded voice file is empty");
        return None;
    }

    Some(temp)
}

/// Converts audio to WAV format (16kHz mono) for whisper-cli.
/// Telegram voice messages use OGG Opus which whisper.cpp cannot decode directly.
async fn convert_to_wav(input_path: &std::path::Path) -> Result<tempfile::NamedTempFile, String> {
    let wav_temp = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .map_err(|e| format!("Failed to create temp WAV file: {}", e))?;

    let output = tokio::process::Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input_path)
        .arg("-ar")
        .arg("16000")
        .arg("-ac")
        .arg("1")
        .arg("-f")
        .arg("wav")
        .arg(wav_temp.path())
        .output()
        .await
        .map_err(|e| format!("Failed to run ffmpeg: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg conversion failed: {}", stderr));
    }

    log::debug!("Converted audio to WAV: {:?}", wav_temp.path());
    Ok(wav_temp)
}

fn resolve_model_path() -> String {
    std::env::var("RUBBERDUX_WHISPER_MODEL").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        let default_path = format!(
            "{}/.local/share/whisper-cpp/models/ggml-large-v3-turbo.bin",
            home
        );
        if std::path::Path::new(&default_path).exists() {
            return default_path;
        }
        let brew_path = "/opt/homebrew/opt/whisper-cpp/share/whisper-cpp/for-tests-ggml-tiny.bin";
        if std::path::Path::new(brew_path).exists() {
            return brew_path.to_owned();
        }
        default_path
    })
}

async fn transcribe(audio_path: &std::path::Path) -> Result<String, String> {
    let model_path = resolve_model_path();
    let language = std::env::var("RUBBERDUX_WHISPER_LANGUAGE")
        .unwrap_or_else(|_| DEFAULT_WHISPER_LANGUAGE.into());

    let wav_file = convert_to_wav(audio_path).await?;

    // Use JSON full output for word-level timestamps
    let json_output = tempfile::Builder::new()
        .suffix("")
        .tempfile()
        .map_err(|e| format!("Failed to create temp JSON file: {}", e))?;
    let json_output_base = json_output.path().to_string_lossy().to_string();

    let output = tokio::process::Command::new("whisper-cli")
        .arg("-m")
        .arg(&model_path)
        .arg("-l")
        .arg(&language)
        .arg("-ojf")
        .arg("-of")
        .arg(&json_output_base)
        .arg("-f")
        .arg(wav_file.path())
        .output()
        .await
        .map_err(|e| format!("Failed to run whisper-cli: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("whisper-cli failed: {}", stderr));
    }

    let json_path = format!("{}.json", json_output_base);
    let json_content = std::fs::read_to_string(&json_path)
        .map_err(|e| format!("Failed to read whisper JSON output: {}", e))?;
    let _ = std::fs::remove_file(&json_path);

    let transcript = parse_whisper_json(&json_content)?;
    log::debug!("Whisper transcript with timestamps: {}", transcript);
    Ok(transcript)
}

fn parse_whisper_json(json_str: &str) -> Result<String, String> {
    let data: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("Failed to parse whisper JSON: {}", e))?;

    let transcription = data["transcription"]
        .as_array()
        .ok_or("No transcription array in whisper output")?;

    let mut result = String::from("<voice-transcript>\n");

    for segment in transcription {
        let tokens = match segment["tokens"].as_array() {
            Some(t) => t,
            None => continue,
        };

        for token in tokens {
            let text = token["text"].as_str().unwrap_or("");

            // Skip special tokens like [_BEG_], [_TT_*], etc.
            if text.starts_with("[_") {
                continue;
            }

            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Parse timestamp: "00:00:01,220" → seconds as f64
            let from_ts = token["timestamps"]["from"]
                .as_str()
                .and_then(parse_timestamp);

            match from_ts {
                Some(t) => result.push_str(&format!("<w t=\"{:.2}\">{}</w>\n", t, trimmed)),
                None => result.push_str(&format!("{}\n", trimmed)),
            }
        }
    }

    result.push_str("</voice-transcript>");
    Ok(result)
}

fn parse_timestamp(ts: &str) -> Option<f64> {
    // Format: "00:00:01,220" or "00:00:01.220"
    let ts = ts.replace(',', ".");
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours: f64 = parts[0].parse().ok()?;
    let minutes: f64 = parts[1].parse().ok()?;
    let seconds: f64 = parts[2].parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

pub async fn interpret_voice(bot: &Bot, voice: &Voice) -> InterpretedMessage {
    let temp_file = download_to_temp(bot, &voice.file.id, ".ogg").await;

    let text = match temp_file {
        Some(temp) => match transcribe(temp.path()).await {
            Ok(transcription) if !transcription.is_empty() => {
                log::info!("Voice transcription: {}", transcription);
                transcription
            }
            Ok(_) => "<voice-transcript status=\"empty\" />".into(),
            Err(e) => {
                log::error!("Voice transcription failed: {}", e);
                "<voice-transcript status=\"failed\" />".into()
            }
        },
        None => {
            log::error!("Failed to download voice message");
            "<voice-transcript status=\"download-failed\" />".into()
        }
    };

    InterpretedMessage {
        text,
        attachments: vec![],
    }
}

pub async fn interpret_audio(bot: &Bot, audio: &Audio) -> InterpretedMessage {
    let temp_file = download_to_temp(bot, &audio.file.id, ".ogg").await;

    let text = match temp_file {
        Some(temp) => match transcribe(temp.path()).await {
            Ok(transcription) if !transcription.is_empty() => {
                log::info!("Audio transcription: {}", transcription);
                transcription
            }
            Ok(_) => "<audio-transcript status=\"empty\" />".into(),
            Err(e) => {
                log::error!("Audio transcription failed: {}", e);
                "<audio-transcript status=\"failed\" />".into()
            }
        },
        None => {
            log::error!("Failed to download audio message");
            "<audio-transcript status=\"download-failed\" />".into()
        }
    };

    InterpretedMessage {
        text,
        attachments: vec![],
    }
}

pub async fn interpret_video_note(bot: &Bot, video_note: &VideoNote) -> InterpretedMessage {
    let temp_file = download_to_temp(bot, &video_note.file.id, ".mp4").await;

    let text = match temp_file {
        Some(temp) => match transcribe(temp.path()).await {
            Ok(transcription) if !transcription.is_empty() => {
                log::info!("Video note transcription: {}", transcription);
                transcription
            }
            Ok(_) => "<video-note-transcript status=\"empty\" />".into(),
            Err(e) => {
                log::error!("Video note transcription failed: {}", e);
                "<video-note-transcript status=\"failed\" />".into()
            }
        },
        None => {
            log::error!("Failed to download video note");
            "<video-note-transcript status=\"download-failed\" />".into()
        }
    };

    InterpretedMessage {
        text,
        attachments: vec![],
    }
}
