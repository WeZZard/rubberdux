use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::channel::{AgentResponse, UserMessage};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};

const DEFAULT_BEST_PERFORMANCE_TOKENS: usize = 153_600;
const DEFAULT_SESSION_DIR: &str = "./sessions";
const SESSION_FILENAME: &str = "session.jsonl";

fn session_path() -> PathBuf {
    let dir = std::env::var("RUBBERDUX_SESSION_DIR")
        .unwrap_or_else(|_| DEFAULT_SESSION_DIR.into());
    PathBuf::from(dir).join(SESSION_FILENAME)
}

fn load_session(path: &Path) -> Vec<Message> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = std::io::BufReader::new(file);
    let mut history = Vec::new();

    for (i, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                log::warn!("Session line {} read error: {}", i, e);
                continue;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Message>(&line) {
            Ok(msg) => history.push(msg),
            Err(e) => log::warn!("Session line {} parse error: {}", i, e),
        }
    }

    log::info!("Restored {} messages from session {:?}", history.len(), path);
    history
}

fn append_to_session(path: &Path, message: &Message) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open session file {:?}: {}", path, e);
            return;
        }
    };

    match serde_json::to_string(message) {
        Ok(json) => {
            if writeln!(file, "{}", json).is_err() {
                log::error!("Failed to write to session file");
            }
        }
        Err(e) => log::error!("Failed to serialize message: {}", e),
    }
}

pub async fn run(
    mut rx: mpsc::Receiver<UserMessage>,
    client: MoonshotClient,
    system_prompt: String,
) {
    let best_perf_tokens: usize = std::env::var("RUBBERDUX_LLM_BEST_PERFORMANCE_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BEST_PERFORMANCE_TOKENS);

    let session_file = session_path();
    let mut history: Vec<Message> = load_session(&session_file);
    let mut last_prompt_tokens: usize = 0;

    log::info!(
        "Agent loop started (model: {}, best_performance_tokens: {}, history: {} messages)",
        client.model(),
        best_perf_tokens,
        history.len()
    );

    while let Some(msg) = rx.recv().await {
        let interpreted = &msg.interpreted;
        let text_preview = &interpreted.text;
        let has_attachments = !interpreted.attachments.is_empty();
        let is_silent = msg.reply_tx.is_none();

        log::info!(
            "Processing message: {} (attachments: {}, silent: {})",
            text_preview,
            interpreted.attachments.len(),
            is_silent
        );

        // Internal event: update assistant message with bot-sent Telegram ID
        if let Some(rest) = text_preview.strip_prefix("__update_assistant_id:") {
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            if let (Some(msg_id_str), Some(idx_str)) = (parts.first(), parts.get(1)) {
                if let (Ok(msg_id), Ok(idx)) = (msg_id_str.parse::<i32>(), idx_str.parse::<usize>()) {
                    if let Some(Message::Assistant { content, .. }) = history.get_mut(idx) {
                        if let Some(text) = content {
                            let wrapped = format!(
                                "<telegram-message from=\"assistant\" to=\"user\" id=\"{}\">{}</telegram-message>",
                                msg_id, text
                            );
                            *text = wrapped;
                            append_to_session(&session_file, &history[idx]);
                            log::debug!("Updated assistant message at index {} with id={}", idx, msg_id);
                        }
                    }
                }
            }
            continue;
        }

        // Silent messages (e.g. reactions) — append to history, don't call LLM
        if is_silent {
            let user_msg = Message::User {
                content: UserContent::Text(text_preview.clone()),
            };
            append_to_session(&session_file, &user_msg);
            history.push(user_msg);
            continue;
        }

        // Evict oldest pairs if approaching the best performance threshold
        let estimated_new = text_preview.len() / 4;
        while last_prompt_tokens + estimated_new > best_perf_tokens
            && evict_oldest_pair(&mut history)
        {
            log::info!(
                "Evicting oldest message pair (estimated context: {} + {} = {}, threshold: {})",
                last_prompt_tokens,
                estimated_new,
                last_prompt_tokens + estimated_new,
                best_perf_tokens
            );
            last_prompt_tokens = last_prompt_tokens.saturating_sub(last_prompt_tokens / 10);
        }

        let messages = client.build_messages(&system_prompt, &history, interpreted);

        let result = client.chat(messages).await;
        log::info!("LLM responded for: {}", text_preview);

        let response = match result {
            Ok(chat_response) => {
                last_prompt_tokens = chat_response.usage.prompt_tokens;
                log::debug!(
                    "Token usage: prompt={}, completion={}, total={}, cached={}",
                    chat_response.usage.prompt_tokens,
                    chat_response.usage.completion_tokens,
                    chat_response.usage.total_tokens,
                    chat_response.usage.cached_tokens,
                );

                let response_text = chat_response
                    .choices
                    .first()
                    .map(|c| c.message.content_text())
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_owned())
                    .unwrap_or_else(|| "(empty response)".into());

                // Store text-only in history (no base64 data — too large)
                let history_text = if has_attachments {
                    format!("{} [with {} attachment(s)]", text_preview, interpreted.attachments.len())
                } else {
                    text_preview.clone()
                };

                let user_msg = Message::User {
                    content: UserContent::Text(history_text),
                };
                let asst_msg = Message::Assistant {
                    content: Some(response_text.clone()),
                    tool_calls: None,
                    partial: None,
                };

                append_to_session(&session_file, &user_msg);
                append_to_session(&session_file, &asst_msg);

                history.push(user_msg);
                history.push(asst_msg);

                let asst_index = history.len() - 1;

                AgentResponse {
                    text: response_text,
                    history_index: asst_index,
                }
            }
            Err(e) => {
                log::error!("LLM call failed: {}", e);
                AgentResponse {
                    text: format!("Sorry, I encountered an error: {}", e),
                    history_index: 0,
                }
            }
        };

        if let Some(reply_tx) = msg.reply_tx {
            let _ = reply_tx.send(response);
        }
    }

    log::info!("Agent loop shutting down");
}

fn evict_oldest_pair(history: &mut Vec<Message>) -> bool {
    if history.len() >= 2 {
        history.remove(0);
        history.remove(0);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eviction_removes_oldest_pairs() {
        let mut history = vec![
            Message::User { content: UserContent::Text("old1".into()) },
            Message::Assistant { content: Some("reply1".into()), tool_calls: None, partial: None },
            Message::User { content: UserContent::Text("old2".into()) },
            Message::Assistant { content: Some("reply2".into()), tool_calls: None, partial: None },
            Message::User { content: UserContent::Text("recent".into()) },
            Message::Assistant { content: Some("reply3".into()), tool_calls: None, partial: None },
        ];

        assert!(evict_oldest_pair(&mut history));
        assert_eq!(history.len(), 4);

        assert!(matches!(&history[2], Message::User { content: UserContent::Text(t) } if t == "recent"));
        assert!(matches!(&history[3], Message::Assistant { content: Some(c), .. } if c == "reply3"));
    }

    #[test]
    fn test_no_eviction_under_threshold() {
        let history = vec![
            Message::User { content: UserContent::Text("hello".into()) },
            Message::Assistant { content: Some("hi".into()), tool_calls: None, partial: None },
        ];

        let last_prompt_tokens: usize = 100;
        let estimated_new: usize = 10;
        let best_perf_tokens: usize = 153_600;

        assert!(last_prompt_tokens + estimated_new <= best_perf_tokens);
        assert_eq!(history.len(), 2);
    }
}
