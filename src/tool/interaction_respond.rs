use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::agent::external::interaction_queue::InteractionQueue;
use crate::agent::external::UIInteractionResponse;
use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct InteractionRespondTool {
    queue: Arc<InteractionQueue>,
}

impl InteractionRespondTool {
    pub fn new(queue: Arc<InteractionQueue>) -> Self {
        Self { queue }
    }
}

impl super::Tool for InteractionRespondTool {
    fn name(&self) -> &str {
        "interaction_respond"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("interaction_respond.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!("Failed to parse arguments: {}", e),
                        is_error: true,
                    };
                }
            };

            let request_id = match args["request_id"].as_str() {
                Some(id) => id.to_string(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: request_id".into(),
                        is_error: true,
                    };
                }
            };

            let response_type = match args["response_type"].as_str() {
                Some(t) => t,
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: response_type".into(),
                        is_error: true,
                    };
                }
            };

            let response = match response_type {
                "select_option" => {
                    let index = match args["index"].as_u64() {
                        Some(i) => i as usize,
                        None => {
                            return ToolOutcome::Immediate {
                                content: "Missing required parameter: index (for select_option)"
                                    .into(),
                                is_error: true,
                            };
                        }
                    };
                    UIInteractionResponse::SelectedOption {
                        request_id: request_id.clone(),
                        index,
                    }
                }
                "approve_plan" => UIInteractionResponse::PlanApproved {
                    request_id: request_id.clone(),
                },
                "reject_plan" => {
                    let feedback = args["feedback"].as_str().unwrap_or("").to_string();
                    UIInteractionResponse::PlanRejected {
                        request_id: request_id.clone(),
                        feedback,
                    }
                }
                "grant_permission" => UIInteractionResponse::PermissionGranted {
                    request_id: request_id.clone(),
                },
                "deny_permission" => {
                    let reason = args["feedback"].as_str().unwrap_or("").to_string();
                    UIInteractionResponse::PermissionDenied {
                        request_id: request_id.clone(),
                        reason,
                    }
                }
                _ => {
                    return ToolOutcome::Immediate {
                        content: format!(
                            "Unknown response_type: '{}'. Valid: select_option, approve_plan, \
                             reject_plan, grant_permission, deny_permission",
                            response_type
                        ),
                        is_error: true,
                    };
                }
            };

            if self.queue.resolve(&request_id, response) {
                ToolOutcome::Immediate {
                    content: format!(
                        "Interaction '{}' resolved with {}.",
                        request_id, response_type
                    ),
                    is_error: false,
                }
            } else {
                ToolOutcome::Immediate {
                    content: format!(
                        "Interaction '{}' not found. It may have already been resolved by the user.",
                        request_id
                    ),
                    is_error: true,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::external::interaction_queue::PendingInteraction;
    use crate::agent::external::UIInteractionRequest;
    use crate::tool::Tool;

    fn temp_tool() -> (Arc<InteractionQueue>, InteractionRespondTool) {
        let queue = Arc::new(InteractionQueue::new());
        let tool = InteractionRespondTool::new(queue.clone());
        (queue, tool)
    }

    fn run<F: std::future::Future<Output = ToolOutcome>>(f: F) -> ToolOutcome {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    fn add_question(
        queue: &InteractionQueue,
        request_id: &str,
    ) -> tokio::sync::oneshot::Receiver<UIInteractionResponse> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        queue.add(
            request_id.into(),
            PendingInteraction {
                request: UIInteractionRequest::Question {
                    request_id: request_id.into(),
                    agent_task_id: "test".into(),
                    text: "test question".into(),
                    options: vec![],
                },
                response_tx: tx,
            },
        );
        rx
    }

    #[test]
    fn test_resolve_select_option() {
        let (queue, tool) = temp_tool();
        let mut rx = add_question(&queue, "q-1");

        let outcome = run(tool.execute(
            r#"{"request_id":"q-1","response_type":"select_option","index":2}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error, "unexpected error: {}", content);
                assert!(content.contains("resolved"));
            }
            _ => panic!("Expected Immediate"),
        }

        let response = rx.try_recv().unwrap();
        assert!(matches!(
            response,
            UIInteractionResponse::SelectedOption { index: 2, .. }
        ));
    }

    #[test]
    fn test_resolve_approve_plan() {
        let (queue, tool) = temp_tool();
        let mut rx = add_question(&queue, "p-1");
        let outcome = run(tool.execute(
            r#"{"request_id":"p-1","response_type":"approve_plan"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { is_error, .. } => assert!(!is_error),
            _ => panic!("Expected Immediate"),
        }
        assert!(matches!(
            rx.try_recv().unwrap(),
            UIInteractionResponse::PlanApproved { .. }
        ));
    }

    #[test]
    fn test_resolve_reject_plan() {
        let (queue, tool) = temp_tool();
        let mut rx = add_question(&queue, "p-2");
        let outcome = run(tool.execute(
            r#"{"request_id":"p-2","response_type":"reject_plan","feedback":"missing tests"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { is_error, .. } => assert!(!is_error),
            _ => panic!("Expected Immediate"),
        }
        match rx.try_recv().unwrap() {
            UIInteractionResponse::PlanRejected { feedback, .. } => {
                assert_eq!(feedback, "missing tests")
            }
            other => panic!("Expected PlanRejected, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_unknown_request() {
        let (_queue, tool) = temp_tool();
        let outcome = run(tool.execute(
            r#"{"request_id":"nonexistent","response_type":"approve_plan"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("not found"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_resolve_grant_permission() {
        let (queue, tool) = temp_tool();
        let mut rx = add_question(&queue, "perm-1");
        let outcome = run(tool.execute(
            r#"{"request_id":"perm-1","response_type":"grant_permission"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { is_error, .. } => assert!(!is_error),
            _ => panic!("Expected Immediate"),
        }
        assert!(matches!(
            rx.try_recv().unwrap(),
            UIInteractionResponse::PermissionGranted { .. }
        ));
    }

    #[test]
    fn test_resolve_deny_permission() {
        let (queue, tool) = temp_tool();
        let mut rx = add_question(&queue, "perm-2");
        let outcome = run(tool.execute(
            r#"{"request_id":"perm-2","response_type":"deny_permission","feedback":"not allowed"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { is_error, .. } => assert!(!is_error),
            _ => panic!("Expected Immediate"),
        }
        match rx.try_recv().unwrap() {
            UIInteractionResponse::PermissionDenied { reason, .. } => {
                assert_eq!(reason, "not allowed")
            }
            other => panic!("Expected PermissionDenied, got {:?}", other),
        }
    }

    #[test]
    fn test_missing_request_id() {
        let (_queue, tool) = temp_tool();
        let outcome = run(tool.execute(r#"{"response_type":"approve_plan"}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("request_id"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_missing_response_type() {
        let (_queue, tool) = temp_tool();
        let outcome = run(tool.execute(r#"{"request_id":"q-1"}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("response_type"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_unknown_response_type() {
        let (_queue, tool) = temp_tool();
        let outcome = run(tool.execute(
            r#"{"request_id":"q-1","response_type":"invalid_type"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Unknown response_type"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_select_option_missing_index() {
        let (queue, tool) = temp_tool();
        let _rx = add_question(&queue, "q-2");
        let outcome = run(tool.execute(
            r#"{"request_id":"q-2","response_type":"select_option"}"#,
        ));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("index"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_invalid_json() {
        let (_queue, tool) = temp_tool();
        let outcome = run(tool.execute("not json"));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Failed to parse"));
            }
            _ => panic!("Expected Immediate"),
        }
    }
}
