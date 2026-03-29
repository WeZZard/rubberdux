use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::entry::{Entry, EntryHistory};
use crate::channel::{AgentResponse, ChannelEvent, InternalEvent};
use crate::provider::moonshot::tool::ToolDefinition;
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::BackgroundTaskResult;

/// Result of a single LLM tool turn.
enum TurnResult {
    /// Model stopped (finish_reason="stop"). Send response to user.
    Done { text: String, entry_id: usize },
    /// Sync tools executed, need another LLM call immediately.
    Continue,
    /// Background tasks dispatched. Wait for all to complete.
    Pending { asst_entry_id: usize, task_ids: Vec<String> },
}

/// Runtime state for a group of background tasks from one LLM turn.
struct TaskGroup {
    reply_tx: tokio::sync::mpsc::Sender<AgentResponse>,
    asst_entry_id: usize,
    remaining: usize,
    completed_results: Vec<BackgroundTaskResult>,
    telegram_message_id: Option<i32>,
}

/// A conversation that needs the next LLM step. Persists across select! iterations.
/// Accumulates background task IDs until the model stops, then creates one TaskGroup.
struct ActiveConversation {
    reply_tx: tokio::sync::mpsc::Sender<AgentResponse>,
    /// Background task IDs accumulated across multiple tool turns.
    pending_task_ids: Vec<String>,
    /// The assistant entry that first dispatched background tasks (for TaskGroup).
    first_bg_asst_entry_id: Option<usize>,
    /// Original Telegram message ID for reply-to on background completions.
    telegram_message_id: Option<i32>,
    /// Whether this conversation was triggered by a background task completion.
    is_background_completion: bool,
}

const DEFAULT_BEST_PERFORMANCE_TOKENS: usize = 153_600;
const DEFAULT_SESSION_DIR: &str = "./sessions";
const SESSION_FILENAME: &str = "session.jsonl";

fn session_path() -> PathBuf {
    let dir = std::env::var("RUBBERDUX_SESSION_DIR").unwrap_or_else(|_| DEFAULT_SESSION_DIR.into());
    PathBuf::from(dir).join(SESSION_FILENAME)
}

fn load_session(path: &Path, system_prompt: &str) -> EntryHistory {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            let mut history = EntryHistory::new();
            history.push_system(Message::System {
                content: system_prompt.to_owned(),
            });
            return history;
        }
    };

    let reader = std::io::BufReader::new(file);
    let mut entries: Vec<Entry> = Vec::new();
    let mut legacy_messages: Vec<Message> = Vec::new();
    let mut is_entry_format = false;
    let mut is_legacy_format = false;

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

        if let Ok(entry) = serde_json::from_str::<Entry>(&line) {
            entries.push(entry);
            is_entry_format = true;
            continue;
        }

        if let Ok(msg) = serde_json::from_str::<Message>(&line) {
            legacy_messages.push(msg);
            is_legacy_format = true;
            continue;
        }

        log::warn!("Session line {} parse error", i);
    }

    let mut history = if is_entry_format {
        log::info!(
            "Restored {} entries from session {:?}",
            entries.len(),
            path
        );
        EntryHistory::from_entries(entries)
    } else if is_legacy_format {
        log::info!(
            "Restored {} legacy messages from session {:?}",
            legacy_messages.len(),
            path
        );
        EntryHistory::from_legacy_messages(legacy_messages)
    } else {
        EntryHistory::new()
    };

    // Ensure system message is Entry[0]
    if history.is_empty()
        || !matches!(
            history.entries().first().map(|e| &e.message),
            Some(Message::System { .. })
        )
    {
        let mut new_history = EntryHistory::new();
        new_history.push_system(Message::System {
            content: system_prompt.to_owned(),
        });
        for entry in history.entries() {
            match &entry.message {
                Message::System { .. } => {}
                Message::User { .. } => {
                    new_history.push_user(entry.message.clone());
                }
                Message::Assistant { .. } => {
                    let parent =
                        entry.parent_id.unwrap_or(new_history.last_id().unwrap_or(0));
                    new_history.push_assistant(parent, entry.message.clone());
                }
                Message::Tool { .. } => {
                    let parent =
                        entry.parent_id.unwrap_or(new_history.last_id().unwrap_or(0));
                    new_history.push_tool(parent, entry.message.clone());
                }
            }
        }
        history = new_history;
    }

    history
}

fn append_entry_to_session(path: &Path, entry: &Entry) {
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

    match serde_json::to_string(entry) {
        Ok(json) => {
            if writeln!(file, "{}", json).is_err() {
                log::error!("Failed to write to session file");
            }
        }
        Err(e) => log::error!("Failed to serialize entry: {}", e),
    }
}

async fn run_tool_turn(
    client: &MoonshotClient,
    history: &mut EntryHistory,
    session_file: &Path,
    tools: &[ToolDefinition],
    bg_tx: &tokio::sync::mpsc::Sender<BackgroundTaskResult>,
) -> Result<TurnResult, crate::error::Error> {
    let messages = history.messages();
    let chat_response = client.chat(messages, Some(tools.to_vec())).await?;

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
            return Ok(TurnResult::Done {
                text: "(empty response)".into(),
                entry_id: history.last_id().unwrap_or(0),
            });
        }
    };

    let model_done = choice.finish_reason == "stop";
    let text = choice.message.content_text().to_owned();

    let parent_id = history.last_id().unwrap_or(0);
    let asst_entry_id = history.push_assistant(parent_id, choice.message.clone());
    if !model_done {
        append_entry_to_session(session_file, history.get(asst_entry_id).unwrap());
    }

    if model_done {
        return Ok(TurnResult::Done { text, entry_id: asst_entry_id });
    }

    // Execute tool calls -- separate sync from background
    let tool_calls = match choice.message.tool_calls() {
        Some(tc) => tc.clone(),
        None => return Ok(TurnResult::Continue),
    };

    log::info!("Executing {} tool call(s)", tool_calls.len());

    let mut bg_task_ids = Vec::new();

    for call in &tool_calls {
        log::info!("Tool call: {}({})", call.function.name, call.function.arguments);

        let outcome = if MoonshotClient::is_provider_tool(&call.function.name) {
            let last_user_query = history
                .entries()
                .iter()
                .rev()
                .find_map(|e| match &e.message {
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

        // Format first (borrows outcome)
        let formatted = MoonshotClient::format_tool_outcome(&call.function.name, &outcome)
            .unwrap_or_else(|| crate::tool::format_tool_outcome(&outcome));

        // Check for errors
        if let crate::tool::ToolOutcome::Immediate { ref content, is_error: true } = outcome {
            log::warn!("Tool error: {}", content);
        }

        // Extract background receiver if applicable
        if let crate::tool::ToolOutcome::Background { task_id, receiver, .. } = outcome {
            let bg_tx_clone = bg_tx.clone();
            tokio::spawn(async move {
                if let Ok(result) = receiver.await {
                    let _ = bg_tx_clone.send(result).await;
                }
            });
            bg_task_ids.push(task_id);
        }

        // Create and persist Tool entry
        let tool_name = if MoonshotClient::is_provider_tool(&call.function.name) {
            Some(call.function.name.clone())
        } else {
            None
        };
        let tool_msg = Message::Tool {
            tool_call_id: call.id.clone(),
            name: tool_name,
            content: formatted,
        };
        let tool_entry_id = history.push_tool(asst_entry_id, tool_msg);
        append_entry_to_session(session_file, history.get(tool_entry_id).unwrap());
    }

    if bg_task_ids.is_empty() {
        Ok(TurnResult::Continue)
    } else {
        Ok(TurnResult::Pending { asst_entry_id, task_ids: bg_task_ids })
    }
}

async fn run_until_done_or_pending(
    client: &MoonshotClient,
    history: &mut EntryHistory,
    session_file: &Path,
    tools: &[ToolDefinition],
    bg_tx: &tokio::sync::mpsc::Sender<BackgroundTaskResult>,
) -> Result<TurnResult, crate::error::Error> {
    loop {
        let result = run_tool_turn(client, history, session_file, tools, bg_tx).await?;
        match result {
            TurnResult::Continue => continue,
            other => return Ok(other),
        }
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
    let mut history = load_session(&session_file, &system_prompt);
    let mut last_prompt_tokens: usize = 0;

    log::info!(
        "Agent loop started (model: {}, best_performance_tokens: {}, history: {} messages)",
        client.model(),
        best_perf_tokens,
        history.len()
    );

    let (bg_tx, mut bg_rx) = tokio::sync::mpsc::channel::<BackgroundTaskResult>(32);
    let mut active_groups: HashMap<usize, TaskGroup> = HashMap::new();
    let mut task_to_group: HashMap<String, usize> = HashMap::new();
    let mut active_conversation: Option<ActiveConversation> = None;
    // Notify channel: signals the select! loop that an active conversation needs driving.
    // Send a signal to trigger branch 3. Buffered so sends never block.
    let (conv_notify_tx, mut conv_notify_rx) = tokio::sync::mpsc::channel::<()>(16);

    loop {
        tokio::select! {
            // Branch 1: Channel events (user messages, internal events)
            Some(event) = rx.recv() => {
                match event {
                    ChannelEvent::InternalEvent(internal) => {
                        handle_internal_event(internal, &mut history, &session_file);
                    }
                    ChannelEvent::UserInput { interpreted, reply_tx, telegram_message_id } => {
                        let text_preview = interpreted.text.clone();
                        let has_attachments = !interpreted.attachments.is_empty();
                        let is_silent = reply_tx.is_none();

                        log::info!(
                            "Processing message: {} (attachments: {}, silent: {})",
                            text_preview,
                            interpreted.attachments.len(),
                            is_silent
                        );

                        if is_silent {
                            append_user_message(text_preview, &mut history, &session_file);
                            continue;
                        }

                        let reply_tx = match reply_tx {
                            Some(tx) => tx,
                            None => continue,
                        };

                        evict_if_needed(
                            &mut history,
                            &mut last_prompt_tokens,
                            &text_preview,
                            best_perf_tokens,
                        );

                        let history_text = if has_attachments {
                            format!("{} [with {} attachment(s)]", text_preview, interpreted.attachments.len())
                        } else {
                            text_preview.clone()
                        };
                        append_user_message(history_text, &mut history, &session_file);

                        // If there's an active conversation with pending bg tasks,
                        // finalize its TaskGroup before starting a new conversation.
                        if let Some(prev) = active_conversation.take() {
                            if !prev.pending_task_ids.is_empty() {
                                let asst_entry_id = prev.first_bg_asst_entry_id.unwrap_or(0);
                                for tid in &prev.pending_task_ids {
                                    task_to_group.insert(tid.clone(), asst_entry_id);
                                }
                                active_groups.insert(asst_entry_id, TaskGroup {
                                    reply_tx: prev.reply_tx,
                                    asst_entry_id,
                                    remaining: prev.pending_task_ids.len(),
                                    completed_results: Vec::new(),
                                    telegram_message_id: prev.telegram_message_id,
                                });
                            }
                        }

                        // Start a new conversation — next select! iteration will drive it
                        // telegram_message_id is stored for TaskGroup use (reply-to on bg completion)
                        // but NOT used for the initial response (regular messages don't reply-to)
                        active_conversation = Some(ActiveConversation {
                            reply_tx,
                            pending_task_ids: Vec::new(),
                            first_bg_asst_entry_id: None,
                            telegram_message_id,
                            is_background_completion: false,
                        });
                        let _ = conv_notify_tx.send(()).await;
                    }
                }
            }

            // Branch 2: Background task completions
            Some(bg_result) = bg_rx.recv() => {
                let group_key = match task_to_group.remove(&bg_result.task_id) {
                    Some(k) => k,
                    None => {
                        log::warn!("Received result for unknown task: {}", bg_result.task_id);
                        continue;
                    }
                };

                let group = match active_groups.get_mut(&group_key) {
                    Some(g) => g,
                    None => {
                        log::warn!("No active group for key {}", group_key);
                        continue;
                    }
                };

                log::info!(
                    "Background task {} completed ({} bytes), {}/{} in group",
                    bg_result.task_id,
                    bg_result.content.len(),
                    group.completed_results.len() + 1,
                    group.remaining + group.completed_results.len(),
                );

                group.completed_results.push(bg_result);
                group.remaining -= 1;

                if group.remaining > 0 {
                    continue;
                }

                // Group complete! Batch all results and start a conversation to process them.
                let group = active_groups.remove(&group_key).unwrap();

                for result in &group.completed_results {
                    let msg = Message::User {
                        content: UserContent::Text(format!(
                            "[Background task {} completed]\n{}",
                            result.task_id, result.content
                        )),
                    };
                    let entry_id = history.push_user(msg);
                    append_entry_to_session(&session_file, history.get(entry_id).unwrap());
                }

                // Drive the conversation from the next select! iteration
                active_conversation = Some(ActiveConversation {
                    reply_tx: group.reply_tx,
                    pending_task_ids: Vec::new(),
                    first_bg_asst_entry_id: None,
                    telegram_message_id: group.telegram_message_id,
                    is_background_completion: true,
                });
                let _ = conv_notify_tx.send(()).await;
            }

            // Branch 3: Drive active conversation one step at a time
            Some(()) = conv_notify_rx.recv(), if active_conversation.is_some() => {
                let tools = assemble_tool_definitions(&client);

                let turn_result = run_tool_turn(
                    &client, &mut history, &session_file, &tools, &bg_tx,
                ).await;

                match turn_result {
                    Ok(TurnResult::Done { text, entry_id }) => {
                        let conv = active_conversation.take().unwrap();

                        if conv.pending_task_ids.is_empty() {
                            // No background tasks — truly final
                            let is_final = active_groups.is_empty();
                            let reply_to = if conv.is_background_completion {
                                conv.telegram_message_id
                            } else {
                                None
                            };
                            send_response(&conv.reply_tx, text, entry_id, is_final, reply_to).await;
                        } else {
                            // Background tasks accumulated — create ONE group, send non-final response
                            let asst_entry_id = conv.first_bg_asst_entry_id.unwrap_or(entry_id);
                            for tid in &conv.pending_task_ids {
                                task_to_group.insert(tid.clone(), asst_entry_id);
                            }
                            active_groups.insert(asst_entry_id, TaskGroup {
                                reply_tx: conv.reply_tx.clone(),
                                asst_entry_id,
                                remaining: conv.pending_task_ids.len(),
                                completed_results: Vec::new(),
                                telegram_message_id: conv.telegram_message_id,
                            });
                            send_response(&conv.reply_tx, text, entry_id, false, None).await;
                        }
                        log::info!("LLM conversation completed");
                    }
                    Ok(TurnResult::Continue) => {
                        // Sync tools executed — need another LLM call.
                        let _ = conv_notify_tx.send(()).await;
                    }
                    Ok(TurnResult::Pending { asst_entry_id, task_ids }) => {
                        // Accumulate background tasks — don't create a group yet.
                        // The model needs to keep running until it stops.
                        let conv = active_conversation.as_mut().unwrap();
                        if conv.first_bg_asst_entry_id.is_none() {
                            conv.first_bg_asst_entry_id = Some(asst_entry_id);
                        }
                        conv.pending_task_ids.extend(task_ids);
                        let _ = conv_notify_tx.send(()).await;
                    }
                    Err(e) => {
                        let conv = active_conversation.take().unwrap();
                        log::error!("LLM call failed: {}", e);
                        send_response(
                            &conv.reply_tx,
                            format!("Sorry, I encountered an error: {}", e),
                            history.last_id().unwrap_or(0),
                            true,
                            None,
                        ).await;
                    }
                }
            }

            else => break,
        }
    }

    log::info!("Agent loop shutting down");
}

async fn send_response(
    tx: &tokio::sync::mpsc::Sender<AgentResponse>,
    text: String,
    entry_id: usize,
    is_final: bool,
    reply_to_message_id: Option<i32>,
) {
    if !text.is_empty() || is_final {
        let _ = tx
            .send(AgentResponse {
                text,
                entry_id,
                is_final,
                reply_to_message_id,
            })
            .await;
    }
}

fn handle_internal_event(
    event: InternalEvent,
    history: &mut EntryHistory,
    session_file: &Path,
) {
    match event {
        InternalEvent::UpdateAssistantMessageId {
            entry_id,
            message_id,
        } => {
            let updated = if let Some(entry) = history.get_mut(entry_id) {
                if let Message::Assistant { content, .. } = &mut entry.message {
                    if let Some(text) = content {
                        crate::channel::adapter::telegram::inject_assistant_message_id(
                            text, message_id,
                        );
                        log::debug!(
                            "Updated assistant message entry {} with id={}",
                            entry_id,
                            message_id
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };
            if updated {
                if let Some(entry) = history.get(entry_id) {
                    append_entry_to_session(session_file, entry);
                }
            }
        }
    }
}

fn append_user_message(text: String, history: &mut EntryHistory, session_file: &Path) {
    let id = history.push_user(Message::User {
        content: UserContent::Text(text),
    });
    append_entry_to_session(session_file, history.get(id).unwrap());
}

fn evict_if_needed(
    history: &mut EntryHistory,
    last_prompt_tokens: &mut usize,
    text_preview: &str,
    best_perf_tokens: usize,
) {
    let estimated_new = text_preview.len() / 4;
    while *last_prompt_tokens + estimated_new > best_perf_tokens && history.evict_oldest_pair() {
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

/// Assembles the tool definitions for a chat turn: standard defaults
/// with provider-specific overrides applied.
fn assemble_tool_definitions(client: &MoonshotClient) -> Vec<ToolDefinition> {
    let defaults = crate::tool::default_tool_definitions();
    client.override_tool_definitions(defaults).into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eviction_removes_oldest_pairs() {
        let mut history = EntryHistory::new();
        history.push_system(Message::System {
            content: "sys".into(),
        });
        let u1 = history.push_user(Message::User {
            content: UserContent::Text("old1".into()),
        });
        history.push_assistant(
            u1,
            Message::Assistant {
                content: Some("reply1".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        let u2 = history.push_user(Message::User {
            content: UserContent::Text("old2".into()),
        });
        history.push_assistant(
            u2,
            Message::Assistant {
                content: Some("reply2".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        let u3 = history.push_user(Message::User {
            content: UserContent::Text("recent".into()),
        });
        history.push_assistant(
            u3,
            Message::Assistant {
                content: Some("reply3".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );

        assert_eq!(history.len(), 7);
        assert!(history.evict_oldest_pair());
        assert_eq!(history.len(), 5);
    }

    #[test]
    fn test_no_eviction_under_threshold() {
        let mut history = EntryHistory::new();
        history.push_system(Message::System {
            content: "sys".into(),
        });
        history.push_user(Message::User {
            content: UserContent::Text("hello".into()),
        });

        let last_prompt_tokens: usize = 100;
        let estimated_new: usize = 10;
        let best_perf_tokens: usize = 153_600;

        assert!(last_prompt_tokens + estimated_new <= best_perf_tokens);
        assert_eq!(history.len(), 2);
    }
}
