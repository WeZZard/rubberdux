pub mod claude_code;
pub mod codex;
pub mod guardrails;
pub mod interaction_queue;

use std::sync::Arc;

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

fn set_agent_task_id(request: &mut UIInteractionRequest, task_id: &str) {
    match request {
        UIInteractionRequest::Question {
            agent_task_id, ..
        } => *agent_task_id = task_id.into(),
        UIInteractionRequest::PlanApproval {
            agent_task_id, ..
        } => *agent_task_id = task_id.into(),
        UIInteractionRequest::PermissionRequest {
            agent_task_id, ..
        } => *agent_task_id = task_id.into(),
    }
}

fn get_request_id(request: &UIInteractionRequest) -> &str {
    match request {
        UIInteractionRequest::Question { request_id, .. } => request_id,
        UIInteractionRequest::PlanApproval { request_id, .. } => request_id,
        UIInteractionRequest::PermissionRequest { request_id, .. } => request_id,
    }
}

pub fn convention(
    interaction_queue: std::sync::Arc<interaction_queue::InteractionQueue>,
) -> crate::guardrail::Convention {
    crate::guardrail::Convention {
        name: "External Agent Interactions".into(),
        guidance: include_str!("convention.md").into(),
        pre_guardrails: vec![Box::new(guardrails::SafetyGateGuardrail)],
        post_guardrails: vec![Box::new(guardrails::InteractionActionGuardrail::new(
            interaction_queue,
        ))],
    }
}

/// Formats a UIInteractionRequest into a human-readable message for injection
/// into the agent conversation loop.
fn format_interaction_message(request: &UIInteractionRequest, task_id: &str) -> String {
    match request {
        UIInteractionRequest::Question {
            request_id,
            text,
            options,
            ..
        } => {
            let mut msg = format!(
                "[External agent interaction — task {}]\nType: Question\nText: \"{}\"\n",
                task_id, text
            );
            if !options.is_empty() {
                msg.push_str("Options:\n");
                for (i, opt) in options.iter().enumerate() {
                    msg.push_str(&format!("  {}: {} — {}\n", i, opt.label, opt.description));
                }
            }
            msg.push_str(&format!("\nRequest ID: {}\n\n", request_id));
            msg.push_str("Investigate by searching the codebase for evidence. If you find a definitive answer, call interaction_respond. If you cannot determine the answer, let it remain pending for the user.");
            msg
        }
        UIInteractionRequest::PlanApproval {
            request_id,
            plan_text,
            ..
        } => {
            let preview = if plan_text.len() > 2000 {
                &plan_text[..2000]
            } else {
                plan_text.as_str()
            };
            format!(
                "[External agent interaction — plan review]\nAgent task: {}\nRequest ID: {}\n\nPlan:\n{}\n\nSpawn review subagents to check format, consistency, design, and completeness. Present findings to the user. Do not auto-approve.",
                task_id, request_id, preview
            )
        }
        UIInteractionRequest::PermissionRequest {
            request_id,
            description,
            ..
        } => {
            format!(
                "[External agent interaction — permission request]\nAgent task: {}\nRequest ID: {}\nAction: {}\n\nEvaluate if this is a safe operation. If safe, call interaction_respond to grant. If dangerous or uncertain, let it remain pending for the user.",
                task_id, request_id, description
            )
        }
    }
}

/// Spawns a tokio task that drives an external agent event loop.
/// Forwards Completed/Failed to SubagentResult. Logs other events.
/// UI interaction requests are queued and forwarded for notification,
/// and also injected into the parent agent conversation via `input_port`.
pub fn spawn_external_agent_session(
    task_id: String,
    mut event_rx: tokio::sync::mpsc::Receiver<ExternalAgentEvent>,
    cancel: CancellationToken,
    response_tx: tokio::sync::mpsc::Sender<UIInteractionResponse>,
    interaction_queue: Arc<interaction_queue::InteractionQueue>,
    interaction_notify_tx: tokio::sync::mpsc::Sender<UIInteractionRequest>,
    input_port: crate::agent::runtime::port::InputPort,
    recorder: crate::trajectory::SharedTrajectoryRecorder,
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
                            recorder.record(crate::trajectory::TrajectoryEventDraft::new(
                                "external_agent.completed", "agent.external", &task_id,
                            ).with_task_id(Some(task_id.clone()))
                             .with_payload(serde_json::json!({ "result_length": result.len() })));
                            let _ = result_tx.send(SubagentResult {
                                task_id: task_id.clone(),
                                summary: result,
                            });
                            break;
                        }
                        Some(ExternalAgentEvent::Failed { error }) => {
                            recorder.record(crate::trajectory::TrajectoryEventDraft::new(
                                "external_agent.failed", "agent.external", &task_id,
                            ).with_task_id(Some(task_id.clone()))
                             .with_payload(serde_json::json!({ "error": error })));
                            let _ = result_tx.send(SubagentResult {
                                task_id: task_id.clone(),
                                summary: format!("External agent failed: {}", error),
                            });
                            break;
                        }
                        Some(ExternalAgentEvent::UIInteraction(mut request)) => {
                            set_agent_task_id(&mut request, &task_id);
                            let request_id = get_request_id(&request).to_string();

                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                            interaction_queue.add(
                                request_id.clone(),
                                interaction_queue::PendingInteraction {
                                    request: request.clone(),
                                    response_tx: resp_tx,
                                },
                            );

                            recorder.record(crate::trajectory::TrajectoryEventDraft::new(
                                "external_agent.interaction", "agent.external", &task_id,
                            ).with_task_id(Some(task_id.clone()))
                             .with_payload(serde_json::json!({ "request_id": get_request_id(&request) })));

                            // Notify (Telegram will show buttons)
                            let _ = interaction_notify_tx.send(request.clone()).await;

                            // Inject the interaction as a user message into the parent agent loop
                            let injection_text = format_interaction_message(&request, &task_id);
                            let injection_message = crate::provider::moonshot::Message::User {
                                content: crate::provider::moonshot::UserContent::Text(injection_text),
                            };
                            let _ = input_port.send_user_message(
                                injection_message,
                                crate::agent::entry::EntryOrigin::System,
                            ).await;

                            // Spawn waiter: when user responds via button, forward to bridge
                            let response_fwd = response_tx.clone();
                            tokio::spawn(async move {
                                if let Ok(response) = resp_rx.await {
                                    let _ = response_fwd.send(response).await;
                                }
                            });
                        }
                        Some(ExternalAgentEvent::Progress { message }) => {
                            recorder.record(crate::trajectory::TrajectoryEventDraft::new(
                                "external_agent.progress", "agent.external", &task_id,
                            ).with_task_id(Some(task_id.clone()))
                             .with_payload(serde_json::json!({ "message": message })));
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
    use tokio::sync::mpsc;

    fn dummy_input_port() -> crate::agent::runtime::port::InputPort {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        crate::agent::runtime::port::InputPort::new(tx)
    }

    fn spawn_test_session(
        event_rx: mpsc::Receiver<ExternalAgentEvent>,
    ) -> (
        SubagentHandle,
        mpsc::Sender<UIInteractionResponse>,
        Arc<interaction_queue::InteractionQueue>,
        mpsc::Receiver<UIInteractionRequest>,
    ) {
        let cancel = CancellationToken::new();
        let (response_tx, _response_rx) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, notify_rx) = mpsc::channel(8);
        let recorder: crate::trajectory::SharedTrajectoryRecorder =
            std::sync::Arc::new(crate::trajectory::NoopTrajectoryRecorder);
        let handle = spawn_external_agent_session(
            "test".into(),
            event_rx,
            cancel,
            response_tx.clone(),
            queue.clone(),
            notify_tx,
            dummy_input_port(),
            recorder,
        );
        (handle, response_tx, queue, notify_rx)
    }

    #[tokio::test]
    async fn test_spawn_completed() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (handle, _, _, _) = spawn_test_session(event_rx);

        event_tx
            .send(ExternalAgentEvent::Completed {
                result: "done".into(),
            })
            .await
            .unwrap();

        let result = handle.result_rx.await.unwrap();
        assert_eq!(result.task_id, "test");
        assert_eq!(result.summary, "done");
    }

    #[tokio::test]
    async fn test_spawn_failed() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (handle, _, _, _) = spawn_test_session(event_rx);

        event_tx
            .send(ExternalAgentEvent::Failed {
                error: "oops".into(),
            })
            .await
            .unwrap();

        let result = handle.result_rx.await.unwrap();
        assert_eq!(result.task_id, "test");
        assert!(result.summary.contains("oops"));
    }

    #[tokio::test]
    async fn test_spawn_cancel() {
        let (_event_tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (response_tx, _response_rx) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, _notify_rx) = mpsc::channel(8);
        let recorder: crate::trajectory::SharedTrajectoryRecorder =
            std::sync::Arc::new(crate::trajectory::NoopTrajectoryRecorder);
        let handle = spawn_external_agent_session(
            "test-cancel".into(),
            event_rx,
            cancel.clone(),
            response_tx,
            queue,
            notify_tx,
            dummy_input_port(),
            recorder,
        );

        cancel.cancel();

        // The result channel should be dropped (no result sent)
        let result = handle.result_rx.await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_spawn_ui_interaction_queued() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (response_tx, _response_rx) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, mut notify_rx) = mpsc::channel(8);
        let recorder: crate::trajectory::SharedTrajectoryRecorder =
            std::sync::Arc::new(crate::trajectory::NoopTrajectoryRecorder);

        let _handle = spawn_external_agent_session(
            "test-ui".into(),
            event_rx,
            cancel,
            response_tx,
            queue.clone(),
            notify_tx,
            dummy_input_port(),
            recorder,
        );

        // Send a UIInteraction event
        event_tx
            .send(ExternalAgentEvent::UIInteraction(
                UIInteractionRequest::Question {
                    request_id: "q-test".into(),
                    agent_task_id: String::new(),
                    text: "Which option?".into(),
                    options: vec![],
                },
            ))
            .await
            .unwrap();

        // Verify it was queued
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(queue.pending_count(), 1);

        // Verify notification was sent
        let notified = notify_rx.try_recv();
        assert!(notified.is_ok());
    }

    #[tokio::test]
    async fn test_spawn_injects_question() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (response_tx, _response_rx) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, _notify_rx) = mpsc::channel(8);
        let recorder: crate::trajectory::SharedTrajectoryRecorder =
            std::sync::Arc::new(crate::trajectory::NoopTrajectoryRecorder);

        // Create InputPort where we can check injected messages
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(8);
        let input_port = crate::agent::runtime::port::InputPort::new(input_tx);

        let _handle = spawn_external_agent_session(
            "test-inject".into(),
            event_rx,
            cancel,
            response_tx,
            queue,
            notify_tx,
            input_port,
            recorder,
        );

        // Send a UIInteraction event
        event_tx
            .send(ExternalAgentEvent::UIInteraction(
                UIInteractionRequest::Question {
                    request_id: "q-inject".into(),
                    agent_task_id: String::new(),
                    text: "Which database?".into(),
                    options: vec![QuestionOption {
                        label: "PostgreSQL".into(),
                        description: "relational".into(),
                    }],
                },
            ))
            .await
            .unwrap();

        // Wait for injection
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check that a message was injected into the input port
        let event = input_rx.try_recv();
        assert!(event.is_ok(), "Expected injected message on input port");
    }

    #[tokio::test]
    async fn test_spawn_records_progress() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (response_tx, _) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, _) = mpsc::channel(8);
        let input_port = dummy_input_port();
        let recorder = std::sync::Arc::new(crate::trajectory::MemoryTrajectoryRecorder::new());
        let shared_recorder: crate::trajectory::SharedTrajectoryRecorder = recorder.clone();

        let _handle = spawn_external_agent_session(
            "test-progress".into(),
            event_rx,
            cancel,
            response_tx,
            queue,
            notify_tx,
            input_port,
            shared_recorder,
        );

        // Send progress then completed to end the session
        event_tx
            .send(ExternalAgentEvent::Progress {
                message: "step 1 done".into(),
            })
            .await
            .unwrap();
        event_tx
            .send(ExternalAgentEvent::Completed {
                result: "done".into(),
            })
            .await
            .unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let events = recorder.events();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "external_agent.progress"),
            "Expected progress event, got: {:?}",
            events
                .iter()
                .map(|e| &e.event_type)
                .collect::<Vec<_>>()
        );
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "external_agent.completed"),
            "Expected completed event"
        );
    }

    #[tokio::test]
    async fn test_spawn_records_failed() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (response_tx, _) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, _) = mpsc::channel(8);
        let input_port = dummy_input_port();
        let recorder = std::sync::Arc::new(crate::trajectory::MemoryTrajectoryRecorder::new());
        let shared_recorder: crate::trajectory::SharedTrajectoryRecorder = recorder.clone();

        let _handle = spawn_external_agent_session(
            "test-fail".into(),
            event_rx,
            cancel,
            response_tx,
            queue,
            notify_tx,
            input_port,
            shared_recorder,
        );

        event_tx
            .send(ExternalAgentEvent::Failed {
                error: "something broke".into(),
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let events = recorder.events();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "external_agent.failed"),
            "Expected failed event, got: {:?}",
            events
                .iter()
                .map(|e| &e.event_type)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_spawn_records_interaction() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (response_tx, _) = mpsc::channel(8);
        let queue = Arc::new(interaction_queue::InteractionQueue::new());
        let (notify_tx, _notify_rx) = mpsc::channel(8);
        let input_port = dummy_input_port();
        let recorder = std::sync::Arc::new(crate::trajectory::MemoryTrajectoryRecorder::new());
        let shared_recorder: crate::trajectory::SharedTrajectoryRecorder = recorder.clone();

        let _handle = spawn_external_agent_session(
            "test-interaction".into(),
            event_rx,
            cancel,
            response_tx,
            queue,
            notify_tx,
            input_port,
            shared_recorder,
        );

        event_tx
            .send(ExternalAgentEvent::UIInteraction(
                UIInteractionRequest::Question {
                    request_id: "q-rec".into(),
                    agent_task_id: String::new(),
                    text: "Pick one".into(),
                    options: vec![],
                },
            ))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let events = recorder.events();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "external_agent.interaction"),
            "Expected interaction event, got: {:?}",
            events
                .iter()
                .map(|e| &e.event_type)
                .collect::<Vec<_>>()
        );
        // Verify payload contains request_id
        let interaction_event = events
            .iter()
            .find(|e| e.event_type == "external_agent.interaction")
            .unwrap();
        assert_eq!(interaction_event.payload["request_id"], "q-rec");
    }
}
