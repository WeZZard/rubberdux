use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::ToolRegistry;

use super::agent_loop::{AgentLoop, AgentLoopConfig};
use super::compaction::EvictOldestTurns;
use super::port::InputPort;

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
/// The subagent runs its own `AgentLoop` with an isolated history and
/// no session persistence. It subscribes to `context_rx` for live
/// environment updates and respects the `CancellationToken` for early
/// termination.
pub fn spawn_subagent(
    task_id: String,
    client: Arc<MoonshotClient>,
    system_prompt: String,
    initial_prompt: String,
    registry: Arc<ToolRegistry>,
    context_rx: broadcast::Receiver<ContextEvent>,
) -> SubagentHandle {
    let cancel = CancellationToken::new();
    let (result_tx, result_rx) = oneshot::channel();

    let cancel_clone = cancel.clone();
    let task_id_clone = task_id.clone();

    tokio::spawn(async move {
        let config = AgentLoopConfig {
            client,
            registry,
            system_prompt,
            session_path: None,
            token_budget: 128_000,
            cancel: cancel_clone.clone(),
            compaction: Box::new(EvictOldestTurns),
            context_tx: None,
        };

        let (agent_loop, input_port) = AgentLoop::new(config);

        // Send the initial prompt as a UserMessage with a reply channel
        // so the AgentLoop starts a conversation (reply: None would be
        // treated as a silent injection).
        let (reply_tx, _reply_rx) = mpsc::channel(8);
        let initial_msg = Message::User {
            content: UserContent::Text(initial_prompt),
        };
        if input_port
            .send_user_message(initial_msg, Some(reply_tx))
            .await
            .is_err()
        {
            let _ = result_tx.send(SubagentResult {
                task_id: task_id_clone,
                summary: "(failed to send initial prompt)".into(),
            });
            return;
        }

        // Adapter: forward ContextEvent stream into the AgentLoop's InputPort.
        let adapter_cancel = cancel_clone.clone();
        tokio::spawn(adapt_context_events(
            context_rx,
            input_port,
            adapter_cancel,
        ));

        // Drive the loop to completion.
        let summary = agent_loop.run_to_completion().await;

        log::info!("Subagent {} completed", task_id_clone);
        let _ = result_tx.send(SubagentResult {
            task_id: task_id_clone,
            summary,
        });
    });

    SubagentHandle {
        task_id,
        result_rx,
        cancel,
    }
}

/// Adapter that converts `ContextEvent`s from a broadcast receiver into
/// `LoopEvent::ContextUpdate`s sent via the `InputPort`.
async fn adapt_context_events(
    mut context_rx: broadcast::Receiver<ContextEvent>,
    input_port: InputPort,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            event = context_rx.recv() => {
                match event {
                    Ok(ContextEvent::EnvironmentChange(msg)) | Ok(ContextEvent::UserMessage(msg)) => {
                        if input_port.send_context_update(msg).await.is_err() {
                            break; // Loop closed
                        }
                    }
                    Ok(ContextEvent::Cancel) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("Subagent context adapter lagged by {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
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
