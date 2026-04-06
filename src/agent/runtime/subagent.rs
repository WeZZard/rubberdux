use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::provider::moonshot::Message;

/// A context event broadcast to all active subagents.
#[derive(Debug, Clone)]
pub enum ContextEvent {
    /// Environment state changed (timezone, locale, etc.)
    EnvironmentChange(Message),
    /// New user message arrived.
    UserMessage(Message),
    /// Cancellation signal.
    Cancel,
}

/// Handle held by the outer conversation to track a running subagent.
pub struct SubagentHandle {
    pub task_id: String,
    pub result_rx: oneshot::Receiver<SubagentResult>,
    pub cancel: CancellationToken,
}

/// Result returned by a subagent on completion.
pub struct SubagentResult {
    pub task_id: String,
    pub summary: String,
}

/// Spawn a subagent as a tokio task. Returns a handle for tracking.
///
/// The subagent runs its own isolated LLM loop with a private
/// `EntryHistory`. It subscribes to `context_rx` for live environment
/// updates and respects the `CancellationToken` for early termination.
pub fn spawn_subagent(
    task_id: String,
    client: std::sync::Arc<crate::provider::moonshot::MoonshotClient>,
    system_prompt: String,
    initial_prompt: String,
    registry: std::sync::Arc<crate::tool::ToolRegistry>,
    context_rx: broadcast::Receiver<ContextEvent>,
    bg_tx: tokio::sync::mpsc::Sender<crate::tool::BackgroundTaskResult>,
) -> SubagentHandle {
    let cancel = CancellationToken::new();
    let (result_tx, result_rx) = oneshot::channel();

    let cancel_clone = cancel.clone();
    let task_id_clone = task_id.clone();

    tokio::spawn(async move {
        let result = run_subagent(
            &client,
            system_prompt,
            initial_prompt,
            &registry,
            context_rx,
            cancel_clone,
            bg_tx,
            &task_id_clone,
        ).await;
        let _ = result_tx.send(result);
    });

    SubagentHandle {
        task_id,
        result_rx,
        cancel,
    }
}

async fn run_subagent(
    client: &crate::provider::moonshot::MoonshotClient,
    system_prompt: String,
    initial_prompt: String,
    registry: &crate::tool::ToolRegistry,
    mut context_rx: broadcast::Receiver<ContextEvent>,
    cancel: CancellationToken,
    _bg_tx: tokio::sync::mpsc::Sender<crate::tool::BackgroundTaskResult>,
    task_id: &str,
) -> SubagentResult {
    use crate::agent::entry::EntryHistory;
    use crate::provider::moonshot::UserContent;

    let mut history = EntryHistory::new();
    history.push_system(Message::System {
        content: system_prompt,
    });
    history.push_user(Message::User {
        content: UserContent::Text(initial_prompt),
    });

    let tools = registry.definitions();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log::info!("Subagent {} cancelled", task_id);
                return SubagentResult {
                    task_id: task_id.to_owned(),
                    summary: "(cancelled)".into(),
                };
            }

            event = context_rx.recv() => {
                match event {
                    Ok(ContextEvent::EnvironmentChange(msg)) | Ok(ContextEvent::UserMessage(msg)) => {
                        history.push_user(msg);
                    }
                    Ok(ContextEvent::Cancel) => {
                        log::info!("Subagent {} received cancel event", task_id);
                        return SubagentResult {
                            task_id: task_id.to_owned(),
                            summary: "(cancelled)".into(),
                        };
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("Subagent {} lagged by {} context events", task_id, n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Context stream closed, continue without updates
                    }
                }
            }

            result = async {
                let messages = history.messages();
                client.chat(messages, Some(tools.clone())).await
            } => {
                match result {
                    Ok(response) => {
                        let choice = match response.choices.first() {
                            Some(c) => c,
                            None => {
                                return SubagentResult {
                                    task_id: task_id.to_owned(),
                                    summary: "(empty response)".into(),
                                };
                            }
                        };

                        let text = choice.message.content_text().to_owned();
                        let parent_id = history.last_id().unwrap_or(0);
                        let asst_entry_id = history.push_assistant(parent_id, choice.message.clone());

                        if choice.finish_reason == "stop" {
                            log::info!("Subagent {} completed", task_id);
                            return SubagentResult {
                                task_id: task_id.to_owned(),
                                summary: text,
                            };
                        }

                        // Execute tool calls
                        if let Some(tool_calls) = choice.message.tool_calls() {
                            let tool_results = super::chat::execute_tool_calls(tool_calls, registry).await;
                            for (call, outcome) in tool_results {
                                let formatted = crate::tool::format_tool_outcome(&outcome);
                                let tool_msg = Message::Tool {
                                    tool_call_id: call.id.clone(),
                                    name: None,
                                    content: formatted,
                                };
                                history.push_tool(asst_entry_id, tool_msg);
                            }
                        }
                        // Loop back for next LLM call
                    }
                    Err(e) => {
                        log::error!("Subagent {} LLM error: {}", task_id, e);
                        return SubagentResult {
                            task_id: task_id.to_owned(),
                            summary: format!("Subagent failed: {}", e),
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_context_event_broadcast() {
        let (tx, _) = broadcast::channel::<ContextEvent>(16);
        let mut rx1 = tx.subscribe();
        let mut rx2 = tx.subscribe();

        let msg = Message::System { content: "tz changed".into() };
        tx.send(ContextEvent::EnvironmentChange(msg)).unwrap();

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();

        match (e1, e2) {
            (ContextEvent::EnvironmentChange(m1), ContextEvent::EnvironmentChange(m2)) => {
                assert_eq!(m1.content_text(), "tz changed");
                assert_eq!(m2.content_text(), "tz changed");
            }
            _ => panic!("Expected EnvironmentChange events"),
        }
    }

    #[tokio::test]
    async fn test_subagent_cancellation() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = cancel_clone.cancelled() => {
                    SubagentResult {
                        task_id: "test".into(),
                        summary: "(cancelled)".into(),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    SubagentResult {
                        task_id: "test".into(),
                        summary: "should not reach".into(),
                    }
                }
            }
        });

        // Cancel immediately
        cancel.cancel();

        let result = handle.await.unwrap();
        assert_eq!(result.task_id, "test");
        assert!(result.summary.contains("cancelled"));
    }
}
