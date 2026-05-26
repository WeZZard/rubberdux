pub mod claude_code;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::runtime::subagent::{SubagentHandle, SubagentResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UIInteractionRequest {
    Question {
        request_id: String,
        agent_task_id: String,
        text: String,
        options: Vec<QuestionOption>,
    },
    PlanApproval {
        request_id: String,
        agent_task_id: String,
        plan_text: String,
    },
    PermissionRequest {
        request_id: String,
        agent_task_id: String,
        description: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UIInteractionResponse {
    SelectedOption { request_id: String, index: usize },
    PlanApproved { request_id: String },
    PlanRejected { request_id: String, feedback: String },
    PermissionGranted { request_id: String },
    PermissionDenied { request_id: String, reason: String },
}

#[derive(Debug)]
pub enum ExternalAgentEvent {
    UIInteraction(UIInteractionRequest),
    Progress { message: String },
    Completed { result: String },
    Failed { error: String },
}

/// Spawns a tokio task that drives an external agent event loop.
/// Forwards Completed/Failed to SubagentResult. Logs other events.
pub fn spawn_external_agent_session(
    task_id: String,
    mut event_rx: tokio::sync::mpsc::Receiver<ExternalAgentEvent>,
    cancel: CancellationToken,
) -> SubagentHandle {
    let (result_tx, result_rx) = oneshot::channel();
    let cancel_clone = cancel.clone();
    let task_id_for_handle = task_id.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    match event {
                        Some(ExternalAgentEvent::Completed { result }) => {
                            let _ = result_tx.send(SubagentResult {
                                task_id: task_id.clone(),
                                summary: result,
                            });
                            break;
                        }
                        Some(ExternalAgentEvent::Failed { error }) => {
                            let _ = result_tx.send(SubagentResult {
                                task_id: task_id.clone(),
                                summary: format!("External agent failed: {}", error),
                            });
                            break;
                        }
                        Some(ExternalAgentEvent::UIInteraction(request)) => {
                            log::info!("[external:{}] UI interaction received (not yet handled): {:?}", task_id, request);
                        }
                        Some(ExternalAgentEvent::Progress { message }) => {
                            log::info!("[external:{}] {}", task_id, message);
                        }
                        None => {
                            let _ = result_tx.send(SubagentResult {
                                task_id: task_id.clone(),
                                summary: "External agent session ended".into(),
                            });
                            break;
                        }
                    }
                }
                _ = cancel_clone.cancelled() => {
                    log::info!("[external:{}] cancelled", task_id);
                    break;
                }
            }
        }
    });

    SubagentHandle {
        task_id: task_id_for_handle,
        result_rx,
        cancel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_completed() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = spawn_external_agent_session("test-1".into(), event_rx, cancel);

        event_tx
            .send(ExternalAgentEvent::Completed {
                result: "done".into(),
            })
            .await
            .unwrap();

        let result = handle.result_rx.await.unwrap();
        assert_eq!(result.task_id, "test-1");
        assert_eq!(result.summary, "done");
    }

    #[tokio::test]
    async fn test_spawn_failed() {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = spawn_external_agent_session("test-2".into(), event_rx, cancel);

        event_tx
            .send(ExternalAgentEvent::Failed {
                error: "oops".into(),
            })
            .await
            .unwrap();

        let result = handle.result_rx.await.unwrap();
        assert_eq!(result.task_id, "test-2");
        assert!(result.summary.contains("oops"));
    }

    #[tokio::test]
    async fn test_spawn_cancel() {
        let (_event_tx, event_rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();
        let handle = spawn_external_agent_session("test-3".into(), event_rx, cancel.clone());

        cancel.cancel();

        // The result channel should be dropped (no result sent)
        let result = handle.result_rx.await;
        assert!(result.is_err());
    }
}
