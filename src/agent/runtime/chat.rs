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
                            inject_assistant_message_id(text, msg_id);
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

        // Store user message in history
        let history_text = if has_attachments {
            format!("{} [with {} attachment(s)]", text_preview, interpreted.attachments.len())
        } else {
            text_preview.clone()
        };

        let user_msg = Message::User {
            content: UserContent::Text(history_text),
        };
        append_to_session(&session_file, &user_msg);
        history.push(user_msg);

        // Tool use loop: call LLM, execute tools, repeat until finish_reason="stop"
        let tools = crate::tool::load_tool_definitions(&crate::tool::tools_dir());
        let reply_tx = msg.reply_tx;

        loop {
            let messages = client.build_messages_from_history(&system_prompt, &history);

            let result = client.chat(messages, Some(tools.clone())).await;

            match result {
                Ok(chat_response) => {
                    last_prompt_tokens = chat_response.usage.prompt_tokens;
                    log::debug!(
                        "Token usage: prompt={}, completion={}, total={}, cached={}",
                        chat_response.usage.prompt_tokens,
                        chat_response.usage.completion_tokens,
                        chat_response.usage.total_tokens,
                        chat_response.usage.cached_tokens,
                    );

                    let choice = match chat_response.choices.first() {
                        Some(c) => c,
                        None => {
                            if let Some(tx) = &reply_tx {
                                let _ = tx.send(AgentResponse {
                                    text: "(empty response)".into(),
                                    history_index: history.len().saturating_sub(1),
                                    is_final: true,
                                }).await;
                            }
                            break;
                        }
                    };

                    let is_final = choice.finish_reason == "stop";
                    let text = choice.message.content_text().to_owned();

                    // Append assistant message to history
                    let asst_msg = choice.message.clone();
                    if !is_final {
                        append_to_session(&session_file, &asst_msg);
                    }
                    history.push(asst_msg);

                    let asst_index = history.len() - 1;

                    // Send text to channel immediately (intermediate or final)
                    if !text.is_empty() {
                        if let Some(tx) = &reply_tx {
                            let _ = tx.send(AgentResponse {
                                text: text.clone(),
                                history_index: asst_index,
                                is_final,
                            }).await;
                        }
                    } else if is_final {
                        // Final response with no text — still signal completion
                        if let Some(tx) = &reply_tx {
                            let _ = tx.send(AgentResponse {
                                text: String::new(),
                                history_index: asst_index,
                                is_final: true,
                            }).await;
                        }
                    }

                    if is_final {
                        log::info!("LLM finished for: {}", text_preview);
                        break;
                    }

                    // Execute tool calls
                    if let Some(tool_calls) = choice.message.tool_calls() {
                        log::info!("Executing {} tool call(s)", tool_calls.len());
                        for call in tool_calls {
                            log::info!("Tool call: {}({})", call.function.name, call.function.arguments);
                            let result = crate::tool::execute_tool(
                                &call.function.name,
                                &call.function.arguments,
                            )
                            .await;

                            if result.is_error {
                                log::warn!("Tool error: {}", result.content);
                            }

                            let tool_msg = Message::Tool {
                                tool_call_id: call.id.clone(),
                                content: result.content,
                            };
                            append_to_session(&session_file, &tool_msg);
                            history.push(tool_msg);
                        }
                    }
                }
                Err(e) => {
                    log::error!("LLM call failed: {}", e);
                    if let Some(tx) = &reply_tx {
                        let _ = tx.send(AgentResponse {
                            text: format!("Sorry, I encountered an error: {}", e),
                            history_index: history.len().saturating_sub(1),
                            is_final: true,
                        }).await;
                    }
                    break;
                }
            }
        }
    }

    log::info!("Agent loop shutting down");
}

/// Injects a Telegram message ID into an assistant message's content.
/// If the content already has a `<telegram-message from="assistant" to="user">` tag,
/// the `id` attribute is inserted into the existing tag.
/// Otherwise, the content is wrapped in a new tag.
pub fn inject_assistant_message_id(text: &mut String, msg_id: i32) {
    let tag = "<telegram-message from=\"assistant\" to=\"user\">";
    if let Some(pos) = text.find(tag) {
        let insert_pos = pos + "<telegram-message from=\"assistant\" to=\"user\"".len();
        text.insert_str(insert_pos, &format!(" id=\"{}\"", msg_id));
    } else {
        *text = format!(
            "<telegram-message from=\"assistant\" to=\"user\" id=\"{}\">{}</telegram-message>",
            msg_id, text
        );
    }
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
            Message::Assistant { content: Some("reply1".into()), reasoning_content: None, tool_calls: None, partial: None },
            Message::User { content: UserContent::Text("old2".into()) },
            Message::Assistant { content: Some("reply2".into()), reasoning_content: None, tool_calls: None, partial: None },
            Message::User { content: UserContent::Text("recent".into()) },
            Message::Assistant { content: Some("reply3".into()), reasoning_content: None, tool_calls: None, partial: None },
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
            Message::Assistant { content: Some("hi".into()), reasoning_content: None, tool_calls: None, partial: None },
        ];

        let last_prompt_tokens: usize = 100;
        let estimated_new: usize = 10;
        let best_perf_tokens: usize = 153_600;

        assert!(last_prompt_tokens + estimated_new <= best_perf_tokens);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn test_inject_id_into_existing_telegram_tag() {
        // Model already wrapped its response in <telegram-message> tags
        let mut text = "<telegram-message from=\"assistant\" to=\"user\">Hello!</telegram-message>".to_owned();
        inject_assistant_message_id(&mut text, 73);

        assert_eq!(
            text,
            "<telegram-message from=\"assistant\" to=\"user\" id=\"73\">Hello!</telegram-message>"
        );
        // Must NOT create nested tags
        assert_eq!(text.matches("<telegram-message").count(), 1);
        assert_eq!(text.matches("</telegram-message>").count(), 1);
    }

    #[test]
    fn test_inject_id_wraps_plain_text() {
        // Model responded with plain text (no telegram-message tag)
        let mut text = "Hello!".to_owned();
        inject_assistant_message_id(&mut text, 73);

        assert_eq!(
            text,
            "<telegram-message from=\"assistant\" to=\"user\" id=\"73\">Hello!</telegram-message>"
        );
    }

    #[test]
    fn test_inject_id_does_not_double_nest() {
        // Simulate what happened before the fix: model wraps, then we wrap again
        let mut text = "<telegram-message from=\"assistant\" to=\"user\">Some content</telegram-message>".to_owned();
        inject_assistant_message_id(&mut text, 42);

        // Should have exactly one opening and one closing tag
        assert_eq!(text.matches("<telegram-message").count(), 1);
        assert_eq!(text.matches("</telegram-message>").count(), 1);
        // Should contain the id
        assert!(text.contains("id=\"42\""));
    }

    #[test]
    fn test_inject_id_preserves_content_with_inner_tags() {
        // Model response that contains other XML-like content inside
        let mut text = "<telegram-message from=\"assistant\" to=\"user\">Here is a <b>bold</b> word</telegram-message>".to_owned();
        inject_assistant_message_id(&mut text, 99);

        assert!(text.contains("id=\"99\""));
        assert!(text.contains("<b>bold</b>"));
        assert_eq!(text.matches("<telegram-message").count(), 1);
    }

    #[test]
    fn test_inject_id_into_history_modifies_in_place() {
        // Simulate the full flow: history has an assistant message, we inject the ID
        let mut history = vec![
            Message::User { content: UserContent::Text("Hello".into()) },
            Message::Assistant {
                content: Some("<telegram-message from=\"assistant\" to=\"user\">Hi there!</telegram-message>".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        ];

        // Simulate __update_assistant_id for index 1, msg_id 55
        if let Some(Message::Assistant { content, .. }) = history.get_mut(1) {
            if let Some(text) = content {
                inject_assistant_message_id(text, 55);
            }
        }

        let asst_content = history[1].content_text();
        assert!(asst_content.contains("id=\"55\""));
        assert_eq!(asst_content.matches("<telegram-message").count(), 1);

        // History length unchanged — modified in place, not appended
        assert_eq!(history.len(), 2);
    }
}
