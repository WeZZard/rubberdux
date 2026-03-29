use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::channel::{AgentResponse, ChannelEvent, InternalEvent};
use crate::provider::moonshot::tool::ToolDefinition;
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};

const DEFAULT_BEST_PERFORMANCE_TOKENS: usize = 153_600;
const DEFAULT_SESSION_DIR: &str = "./sessions";
const SESSION_FILENAME: &str = "session.jsonl";

fn session_path() -> PathBuf {
    let dir = std::env::var("RUBBERDUX_SESSION_DIR").unwrap_or_else(|_| DEFAULT_SESSION_DIR.into());
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

    log::info!(
        "Restored {} messages from session {:?}",
        history.len(),
        path
    );
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
    mut rx: mpsc::Receiver<ChannelEvent>,
    client: Arc<MoonshotClient>,
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

    while let Some(event) = rx.recv().await {
        match event {
            ChannelEvent::InternalEvent(internal) => {
                handle_internal_event(internal, &mut history, &session_file);
                continue;
            }
            ChannelEvent::UserInput {
                interpreted,
                reply_tx,
            } => {
                let text_preview = &interpreted.text;
                let has_attachments = !interpreted.attachments.is_empty();
                let is_silent = reply_tx.is_none();

                log::info!(
                    "Processing message: {} (attachments: {}, silent: {})",
                    text_preview,
                    interpreted.attachments.len(),
                    is_silent
                );

                if is_silent {
                    append_user_message(text_preview.clone(), &mut history, &session_file);
                    continue;
                }

                evict_if_needed(
                    &mut history,
                    &mut last_prompt_tokens,
                    text_preview,
                    best_perf_tokens,
                );

                let history_text = if has_attachments {
                    format!(
                        "{} [with {} attachment(s)]",
                        text_preview,
                        interpreted.attachments.len()
                    )
                } else {
                    text_preview.clone()
                };
                let text_preview = text_preview.clone();
                append_user_message(history_text, &mut history, &session_file);

        // Tool use loop: call LLM, execute tools, repeat until finish_reason="stop"
        let tools = assemble_tool_definitions(&client);

        loop {
            let messages = build_messages(&system_prompt, &history);

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
                            send_response(
                                &reply_tx,
                                "(empty response)".into(),
                                history.len().saturating_sub(1),
                                true,
                            )
                            .await;
                            break;
                        }
                    };

                    let is_final = choice.finish_reason == "stop";
                    let text = choice.message.content_text().to_owned();

                    let asst_msg = choice.message.clone();
                    if !is_final {
                        append_to_session(&session_file, &asst_msg);
                    }
                    history.push(asst_msg);

                    let asst_index = history.len() - 1;

                    send_response(&reply_tx, text, asst_index, is_final).await;

                    if is_final {
                        log::info!("LLM finished for: {}", text_preview);
                        break;
                    }

                    execute_tool_calls(&client, choice, &mut history, &session_file).await;
                }
                Err(e) => {
                    log::error!("LLM call failed: {}", e);
                    send_response(
                        &reply_tx,
                        format!("Sorry, I encountered an error: {}", e),
                        history.len().saturating_sub(1),
                        true,
                    )
                    .await;
                    break;
                }
            }
        }
            } // ChannelEvent::UserInput
        } // match event
    }

    log::info!("Agent loop shutting down");
}

async fn send_response(
    tx: &Option<tokio::sync::mpsc::Sender<AgentResponse>>,
    text: String,
    history_index: usize,
    is_final: bool,
) {
    if let Some(tx) = tx {
        if !text.is_empty() || is_final {
            let _ = tx
                .send(AgentResponse {
                    text,
                    history_index,
                    is_final,
                })
                .await;
        }
    }
}

async fn execute_tool_calls(
    client: &MoonshotClient,
    choice: &crate::provider::moonshot::api::chat::ChatChoice,
    history: &mut Vec<Message>,
    session_file: &Path,
) {
    let tool_calls = match choice.message.tool_calls() {
        Some(tc) => tc,
        None => return,
    };

    log::info!("Executing {} tool call(s)", tool_calls.len());
    for call in tool_calls {
        log::info!("Tool call: {}({})", call.function.name, call.function.arguments);

        let outcome = if MoonshotClient::is_provider_tool(&call.function.name) {
            let last_user_query = history
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::User { content } => Some(match content {
                        UserContent::Text(t) => t.clone(),
                        UserContent::Parts(_) => "(multimodal)".into(),
                    }),
                    _ => None,
                })
                .unwrap_or_default();

            let ctx = crate::provider::moonshot::ToolExecutionContext {
                last_user_query,
                assistant_message: choice.message.clone(),
                tool_call: call.clone(),
            };

            client
                .execute_provider_tool(&call.function.name, &call.function.arguments, &ctx)
                .await
        } else {
            crate::tool::execute_tool(&call.function.name, &call.function.arguments).await
        };

        if let crate::tool::ToolOutcome::Immediate {
            ref content,
            is_error: true,
        } = outcome
        {
            log::warn!("Tool error: {}", content);
        }

        let content = MoonshotClient::format_tool_outcome(&call.function.name, &outcome)
            .unwrap_or_else(|| crate::tool::format_tool_outcome(&outcome));

        let tool_name = if MoonshotClient::is_provider_tool(&call.function.name) {
            Some(call.function.name.clone())
        } else {
            None
        };

        let tool_msg = Message::Tool {
            tool_call_id: call.id.clone(),
            name: tool_name,
            content,
        };
        append_to_session(session_file, &tool_msg);
        history.push(tool_msg);
    }
}

fn handle_internal_event(event: InternalEvent, history: &mut Vec<Message>, session_file: &Path) {
    match event {
        InternalEvent::UpdateAssistantMessageId {
            history_index,
            message_id,
        } => {
            if let Some(Message::Assistant { content, .. }) = history.get_mut(history_index) {
                if let Some(text) = content {
                    crate::channel::adapter::telegram::inject_assistant_message_id(
                        text, message_id,
                    );
                    append_to_session(session_file, &history[history_index]);
                    log::debug!(
                        "Updated assistant message at index {} with id={}",
                        history_index,
                        message_id
                    );
                }
            }
        }
    }
}

fn append_user_message(text: String, history: &mut Vec<Message>, session_file: &Path) {
    let msg = Message::User {
        content: UserContent::Text(text),
    };
    append_to_session(session_file, &msg);
    history.push(msg);
}

fn evict_if_needed(
    history: &mut Vec<Message>,
    last_prompt_tokens: &mut usize,
    text_preview: &str,
    best_perf_tokens: usize,
) {
    let estimated_new = text_preview.len() / 4;
    while *last_prompt_tokens + estimated_new > best_perf_tokens && evict_oldest_pair(history) {
        log::info!(
            "Evicting oldest message pair (estimated context: {} + {} = {}, threshold: {})",
            last_prompt_tokens,
            estimated_new,
            *last_prompt_tokens + estimated_new,
            best_perf_tokens
        );
        *last_prompt_tokens = last_prompt_tokens.saturating_sub(*last_prompt_tokens / 10);
    }
}

fn build_messages(system_prompt: &str, history: &[Message]) -> Vec<Message> {
    let mut msgs = Vec::with_capacity(history.len() + 1);
    msgs.push(Message::System {
        content: system_prompt.to_owned(),
    });
    msgs.extend_from_slice(history);
    msgs
}

/// Assembles the tool definitions for a chat turn: standard defaults
/// with provider-specific overrides applied.
fn assemble_tool_definitions(client: &MoonshotClient) -> Vec<ToolDefinition> {
    let defaults = crate::tool::default_tool_definitions();
    client.override_tool_definitions(defaults).into_values().collect()
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
            Message::User {
                content: UserContent::Text("old1".into()),
            },
            Message::Assistant {
                content: Some("reply1".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
            Message::User {
                content: UserContent::Text("old2".into()),
            },
            Message::Assistant {
                content: Some("reply2".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
            Message::User {
                content: UserContent::Text("recent".into()),
            },
            Message::Assistant {
                content: Some("reply3".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        ];

        assert!(evict_oldest_pair(&mut history));
        assert_eq!(history.len(), 4);

        assert!(
            matches!(&history[2], Message::User { content: UserContent::Text(t) } if t == "recent")
        );
        assert!(
            matches!(&history[3], Message::Assistant { content: Some(c), .. } if c == "reply3")
        );
    }

    #[test]
    fn test_no_eviction_under_threshold() {
        let history = vec![
            Message::User {
                content: UserContent::Text("hello".into()),
            },
            Message::Assistant {
                content: Some("hi".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        ];

        let last_prompt_tokens: usize = 100;
        let estimated_new: usize = 10;
        let best_perf_tokens: usize = 153_600;

        assert!(last_prompt_tokens + estimated_new <= best_perf_tokens);
        assert_eq!(history.len(), 2);
    }
}
