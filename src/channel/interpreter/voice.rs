use std::sync::OnceLock;

use teloxide::prelude::*;
use teloxide::types::{Audio, VideoNote, Voice};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::InterpretedMessage;

static WHISPER_CONTEXT: OnceLock<Option<WhisperContext>> = OnceLock::new();

fn get_whisper_context() -> Option<&'static WhisperContext> {
    WHISPER_CONTEXT
        .get_or_init(|| {
            let model_path = resolve_model_path();
            log::info!("Loading whisper model from: {}", model_path);
            match WhisperContext::new_with_params(&model_path, WhisperContextParameters::default()) {
                Ok(ctx) => {
                    log::info!("Whisper model loaded successfully");
                    Some(ctx)
                }
                Err(e) => {
                    log::error!("Failed to load whisper model from {}: {:?}", model_path, e);
                    None
                }
            }
        })
        .as_ref()
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

async fn download_to_temp(
    bot: &Bot,
    file_id: &str,
    suffix: &str,
) -> Option<tempfile::NamedTempFile> {
    let file = bot.get_file(file_id).await.ok()?;
    let temp = tempfile::Builder::new().suffix(suffix).tempfile().ok()?;

    {
        let mut dst = tokio::fs::File::create(temp.path()).await.ok()?;
        teloxide::net::Download::download_file(bot, &file.path, &mut dst)
            .await
            .ok()?;
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

/// Converts audio to WAV format (16kHz mono) for whisper.
/// Telegram voice messages use OGG Opus which whisper.cpp cannot decode directly.
async fn convert_to_wav(
    input_path: &std::path::Path,
) -> Result<tempfile::NamedTempFile, String> {
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

/// Loads WAV file as f32 PCM samples at 16kHz mono.
fn load_wav_samples(path: &std::path::Path) -> Result<Vec<f32>, String> {
    let reader =
        hound::WavReader::open(path).map_err(|e| format!("Failed to open WAV file: {}", e))?;

    let spec = reader.spec();
    if spec.channels != 1 {
        return Err(format!("Expected mono audio, got {} channels", spec.channels));
    }

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .into_samples::<i16>()
            .filter_map(|s| s.ok())
            .map(|s| s as f32 / i16::MAX as f32)
            .collect(),
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .filter_map(|s| s.ok())
            .collect(),
    };

    Ok(samples)
}

/// Transcribes audio using whisper-rs (in-process, no CLI dependency).
/// Returns XML-formatted transcript with word-level timestamps.
async fn transcribe(audio_path: &std::path::Path) -> Result<String, String> {
    let wav_file = convert_to_wav(audio_path).await?;
    let samples = load_wav_samples(wav_file.path())?;

    // Run whisper on a blocking thread (it's CPU-intensive)
    let result = tokio::task::spawn_blocking(move || transcribe_samples(&samples))
        .await
        .map_err(|e| format!("Transcription task panicked: {}", e))?;

    result
}

fn transcribe_samples(samples: &[f32]) -> Result<String, String> {
    let ctx = get_whisper_context().ok_or("Whisper model not loaded")?;

    let mut state = ctx
        .create_state()
        .map_err(|e| format!("Failed to create whisper state: {:?}", e))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("auto"));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_token_timestamps(true);

    state
        .full(params, samples)
        .map_err(|e| format!("Whisper inference failed: {:?}", e))?;

    let num_segments = state.full_n_segments().map_err(|e| format!("{:?}", e))?;

    let mut result = String::from("<voice-transcript>\n");

    for i in 0..num_segments {
        let num_tokens = state.full_n_tokens(i).map_err(|e| format!("{:?}", e))?;

        for j in 0..num_tokens {
            let token_text = state
                .full_get_token_text(i, j)
                .map_err(|e| format!("{:?}", e))?;

            let trimmed = token_text.trim();
            if trimmed.is_empty() || trimmed.starts_with("[_") {
                continue;
            }

            let token_data = state
                .full_get_token_data(i, j)
                .map_err(|e| format!("{:?}", e))?;

            let t_start = token_data.t0 as f64 * 0.01; // centiseconds to seconds

            result.push_str(&format!("<w t=\"{:.2}\">{}</w>\n", t_start, trimmed));
        }
    }

    result.push_str("</voice-transcript>");
    Ok(result)
}

pub async fn interpret_voice(bot: &Bot, voice: &Voice) -> InterpretedMessage {
    let temp_file = download_to_temp(bot, &voice.file.id, ".ogg").await;

    let text = match temp_file {
        Some(temp) => match transcribe(temp.path()).await {
            Ok(transcription) if !transcription.contains("<w ") => {
                "<voice-transcript status=\"empty\" />".into()
            }
            Ok(transcription) => {
                log::info!("Voice transcription completed");
                transcription
            }
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
            Ok(transcription) if !transcription.contains("<w ") => {
                "<audio-transcript status=\"empty\" />".into()
            }
            Ok(transcription) => {
                log::info!("Audio transcription completed");
                transcription
            }
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
            Ok(transcription) if !transcription.contains("<w ") => {
                "<video-note-transcript status=\"empty\" />".into()
            }
            Ok(transcription) => {
                log::info!("Video note transcription completed");
                transcription
            }
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
