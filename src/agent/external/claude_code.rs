use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;

use crate::error::Error;

use super::{ExternalAgentEvent, QuestionOption, UIInteractionRequest, UIInteractionResponse};

pub struct ClaudeCodeSession {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout_lines: Lines<BufReader<ChildStdout>>,
    event_tx: mpsc::Sender<ExternalAgentEvent>,
    response_rx: mpsc::Receiver<UIInteractionResponse>,
}

impl ClaudeCodeSession {
    /// Spawn a Claude Code session via the bridge script.
    ///
    /// Returns the session and a sender for forwarding UI interaction responses
    /// back to the bridge process.
    pub async fn spawn(
        prompt: &str,
        cwd: &Path,
        event_tx: mpsc::Sender<ExternalAgentEvent>,
    ) -> Result<(Self, mpsc::Sender<UIInteractionResponse>), Error> {
        // Find the bridge script relative to the current binary or project root
        let bridge_path = std::env::current_dir()
            .unwrap_or_default()
            .join("scripts/bridge-claude-code/index.mjs");

        let mut child = tokio::process::Command::new("node")
            .arg(&bridge_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .current_dir(cwd)
            .spawn()
            .map_err(|e| Error::Provider(format!("Failed to spawn Claude Code bridge: {}", e)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Provider("Failed to capture bridge stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Provider("Failed to capture bridge stdout".into()))?;

        let (response_tx, response_rx) = mpsc::channel(32);

        let mut session = Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout_lines: BufReader::new(stdout).lines(),
            event_tx,
            response_rx,
        };

        // Send the start command
        let start_cmd = serde_json::json!({
            "type": "start",
            "prompt": prompt,
            "args": [],
            "cwd": cwd.to_string_lossy(),
        });
        session.send_command(&start_cmd).await?;

        Ok((session, response_tx))
    }

    async fn send_command(&mut self, cmd: &serde_json::Value) -> Result<(), Error> {
        let line = serde_json::to_string(cmd)
            .map_err(|e| Error::Provider(format!("Failed to serialize command: {}", e)))?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Provider(format!("Failed to write to bridge stdin: {}", e)))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Provider(format!("Failed to write newline: {}", e)))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| Error::Provider(format!("Failed to flush bridge stdin: {}", e)))?;
        Ok(())
    }

    /// Drive the event loop: read events from bridge stdout, forward to event_tx.
    /// Also receives UI interaction responses and forwards them to the bridge.
    /// Call this from a spawned tokio task.
    pub async fn drive(mut self) {
        loop {
            tokio::select! {
                line = self.stdout_lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            if let Some(event) = parse_bridge_event(&line) {
                                if self.event_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                        }
                        _ => break,
                    }
                }
                response = self.response_rx.recv() => {
                    match response {
                        Some(resp) => {
                            if let Err(e) = self.forward_response(resp).await {
                                log::warn!("Failed to forward response to bridge: {}", e);
                            }
                        }
                        None => {} // channel closed, senders dropped
                    }
                }
            }
        }
        // Bridge process ended — send completion if not already sent
        let _ = self
            .event_tx
            .send(ExternalAgentEvent::Completed {
                result: "Claude Code session ended".into(),
            })
            .await;
    }

    async fn forward_response(&mut self, response: UIInteractionResponse) -> Result<(), Error> {
        let cmd = match response {
            UIInteractionResponse::SelectedOption { request_id, index } => {
                serde_json::json!({"type": "answer_question", "requestId": request_id, "selectedIndex": index})
            }
            UIInteractionResponse::PlanApproved { request_id } => {
                serde_json::json!({"type": "approve_plan", "requestId": request_id})
            }
            UIInteractionResponse::PlanRejected {
                request_id,
                feedback,
            } => {
                serde_json::json!({"type": "reject_plan", "requestId": request_id, "feedback": feedback})
            }
            UIInteractionResponse::PermissionGranted { request_id } => {
                serde_json::json!({"type": "approve_plan", "requestId": request_id})
            }
            UIInteractionResponse::PermissionDenied { request_id, reason } => {
                serde_json::json!({"type": "reject_plan", "requestId": request_id, "feedback": reason})
            }
        };
        self.send_command(&cmd).await
    }

    pub async fn cancel(&mut self) {
        let _ = self
            .send_command(&serde_json::json!({"type": "cancel"}))
            .await;
        let _ = self.child.kill().await;
    }
}

fn parse_bridge_event(line: &str) -> Option<ExternalAgentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = v["type"].as_str()?;

    match event_type {
        "completed" => Some(ExternalAgentEvent::Completed {
            result: v["result"].as_str().unwrap_or("").into(),
        }),
        "failed" => Some(ExternalAgentEvent::Failed {
            error: v["error"].as_str().unwrap_or("unknown error").into(),
        }),
        "progress" => Some(ExternalAgentEvent::Progress {
            message: v["message"].as_str().unwrap_or("").into(),
        }),
        "ask_user_question" => {
            let request_id = v["requestId"].as_str().unwrap_or("").into();
            let text = v["text"].as_str().unwrap_or("").into();
            let options = v["questions"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|q| {
                            q["options"].as_array().map(|opts| {
                                opts.iter()
                                    .filter_map(|o| {
                                        Some(QuestionOption {
                                            label: o["label"].as_str()?.into(),
                                            description: o["description"]
                                                .as_str()
                                                .unwrap_or("")
                                                .into(),
                                        })
                                    })
                                    .collect::<Vec<_>>()
                            })
                        })
                        .flatten()
                        .collect()
                })
                .unwrap_or_default();

            Some(ExternalAgentEvent::UIInteraction(
                UIInteractionRequest::Question {
                    request_id,
                    agent_task_id: String::new(),
                    text,
                    options,
                },
            ))
        }
        "plan_review" => {
            let request_id = v["requestId"].as_str().unwrap_or("").into();
            let plan_text = v["planText"].as_str().unwrap_or("").into();
            Some(ExternalAgentEvent::UIInteraction(
                UIInteractionRequest::PlanApproval {
                    request_id,
                    agent_task_id: String::new(),
                    plan_text,
                },
            ))
        }
        _ => {
            log::debug!("Unknown bridge event type: {}", event_type);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_completed_event() {
        let line = r#"{"type":"completed","result":"all tests pass"}"#;
        match parse_bridge_event(line) {
            Some(ExternalAgentEvent::Completed { result }) => {
                assert_eq!(result, "all tests pass");
            }
            other => panic!("Expected Completed, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_failed_event() {
        let line = r#"{"type":"failed","error":"session crashed"}"#;
        match parse_bridge_event(line) {
            Some(ExternalAgentEvent::Failed { error }) => {
                assert_eq!(error, "session crashed");
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_progress_event() {
        let line = r#"{"type":"progress","message":"reading file"}"#;
        match parse_bridge_event(line) {
            Some(ExternalAgentEvent::Progress { message }) => {
                assert_eq!(message, "reading file");
            }
            other => panic!("Expected Progress, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_ask_question_event() {
        let line = r#"{"type":"ask_user_question","requestId":"q-123","text":"Which approach?","questions":[{"options":[{"label":"A","description":"first"},{"label":"B","description":"second"}]}]}"#;
        match parse_bridge_event(line) {
            Some(ExternalAgentEvent::UIInteraction(UIInteractionRequest::Question {
                request_id,
                text,
                options,
                ..
            })) => {
                assert_eq!(request_id, "q-123");
                assert_eq!(text, "Which approach?");
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].label, "A");
                assert_eq!(options[1].label, "B");
            }
            other => panic!("Expected UIInteraction Question, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_plan_review_event() {
        let line = r##"{"type":"plan_review","requestId":"p-456","planText":"# My Plan\n\nDo the thing."}"##;
        match parse_bridge_event(line) {
            Some(ExternalAgentEvent::UIInteraction(UIInteractionRequest::PlanApproval {
                request_id,
                plan_text,
                ..
            })) => {
                assert_eq!(request_id, "p-456");
                assert!(plan_text.contains("My Plan"));
            }
            other => panic!("Expected UIInteraction PlanApproval, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_event() {
        let line = r#"{"type":"unknown_type","data":"something"}"#;
        assert!(parse_bridge_event(line).is_none());
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_bridge_event("not json").is_none());
    }

    #[test]
    fn test_forward_response_json_selected_option() {
        let response = UIInteractionResponse::SelectedOption {
            request_id: "q-1".into(),
            index: 2,
        };
        let cmd = match response {
            UIInteractionResponse::SelectedOption { request_id, index } => {
                serde_json::json!({"type": "answer_question", "requestId": request_id, "selectedIndex": index})
            }
            _ => unreachable!(),
        };
        assert_eq!(cmd["type"], "answer_question");
        assert_eq!(cmd["requestId"], "q-1");
        assert_eq!(cmd["selectedIndex"], 2);
    }

    #[test]
    fn test_forward_response_json_plan_approved() {
        let response = UIInteractionResponse::PlanApproved {
            request_id: "p-1".into(),
        };
        let cmd = match response {
            UIInteractionResponse::PlanApproved { request_id } => {
                serde_json::json!({"type": "approve_plan", "requestId": request_id})
            }
            _ => unreachable!(),
        };
        assert_eq!(cmd["type"], "approve_plan");
        assert_eq!(cmd["requestId"], "p-1");
    }

    #[test]
    fn test_forward_response_json_plan_rejected() {
        let response = UIInteractionResponse::PlanRejected {
            request_id: "p-2".into(),
            feedback: "needs more detail".into(),
        };
        let cmd = match response {
            UIInteractionResponse::PlanRejected {
                request_id,
                feedback,
            } => {
                serde_json::json!({"type": "reject_plan", "requestId": request_id, "feedback": feedback})
            }
            _ => unreachable!(),
        };
        assert_eq!(cmd["type"], "reject_plan");
        assert_eq!(cmd["requestId"], "p-2");
        assert_eq!(cmd["feedback"], "needs more detail");
    }

    #[test]
    fn test_forward_response_json_permission_granted() {
        let response = UIInteractionResponse::PermissionGranted {
            request_id: "perm-1".into(),
        };
        let cmd = match response {
            UIInteractionResponse::PermissionGranted { request_id } => {
                serde_json::json!({"type": "approve_plan", "requestId": request_id})
            }
            _ => unreachable!(),
        };
        assert_eq!(cmd["type"], "approve_plan");
        assert_eq!(cmd["requestId"], "perm-1");
    }

    #[test]
    fn test_forward_response_json_permission_denied() {
        let response = UIInteractionResponse::PermissionDenied {
            request_id: "perm-2".into(),
            reason: "not allowed".into(),
        };
        let cmd = match response {
            UIInteractionResponse::PermissionDenied { request_id, reason } => {
                serde_json::json!({"type": "reject_plan", "requestId": request_id, "feedback": reason})
            }
            _ => unreachable!(),
        };
        assert_eq!(cmd["type"], "reject_plan");
        assert_eq!(cmd["requestId"], "perm-2");
        assert_eq!(cmd["feedback"], "not allowed");
    }
}
