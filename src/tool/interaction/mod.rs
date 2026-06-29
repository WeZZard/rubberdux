//! Agent-facing tools that let an agent raise the unified interaction vocabulary.
//! Each tool builds the corresponding [`AgentInteraction`], enqueues it on the
//! [`InteractionQueue`], records a trajectory event, awaits the response, and
//! returns the resolved [`InteractionResponse`] as the tool result.
//!
//! See `docs/tool/interaction.md` for the conceptual model and design rationale.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::agent::external::interaction_queue::{InteractionQueue, PendingInteraction};
use crate::agent::interaction::{
    AgentInteraction, ApprovalFlavor, ChoiceOption, InteractionResponse, PreviewArtifact,
};
use crate::provider::kimi_for_coding::tool::ToolDefinition;
use crate::trajectory::{SharedTrajectoryRecorder, TrajectoryEventDraft};

use super::{Tool, ToolOutcome};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("req_{:x}", ts & 0xFFFF_FFFF_FFFF)
}

/// Enqueues `interaction` on the legacy `InteractionQueue`, awaits the response
/// via oneshot, converts the legacy response back to the unified vocabulary, and
/// returns it. Bridges the unified request/response vocabulary to the existing
/// queue without replacing it.
///
/// Returns `Err(String)` if the oneshot receiver is dropped before a response
/// arrives (i.e. the queue owner was torn down).
async fn enqueue_and_await(
    queue: &InteractionQueue,
    interaction: AgentInteraction,
) -> Result<InteractionResponse, String> {
    use crate::agent::external::UIInteractionRequest;
    use tokio::sync::oneshot;

    let request_id = interaction.request_id().to_owned();
    let legacy_request: UIInteractionRequest = interaction.into();

    let (tx, rx) = oneshot::channel();
    queue.add(
        request_id,
        PendingInteraction {
            request: legacy_request,
            response_tx: tx,
        },
    );

    let legacy_response = rx.await.map_err(|_| "interaction cancelled: no response received".to_owned())?;
    Ok(legacy_response.into())
}

// ---------------------------------------------------------------------------
// RequestApprovalTool
// ---------------------------------------------------------------------------

/// Tool that raises an [`AgentInteraction::Approval`], enqueues it, and awaits
/// the user's yes/no decision. See `docs/tool/interaction.md`.
pub struct RequestApprovalTool {
    queue: Arc<InteractionQueue>,
    recorder: SharedTrajectoryRecorder,
    app_id: String,
}

impl RequestApprovalTool {
    pub fn new(queue: Arc<InteractionQueue>, recorder: SharedTrajectoryRecorder, app_id: impl Into<String>) -> Self {
        Self { queue, recorder, app_id: app_id.into() }
    }
}

impl Tool for RequestApprovalTool {
    fn name(&self) -> &str {
        "request_approval"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "request_approval",
            "Request the user's approval for an action or plan. Blocks until the user approves or declines.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "flavor": {
                        "type": "string",
                        "enum": ["permission", "plan"],
                        "description": "Whether this is a permission grant for an action ('permission') or a sign-off on a plan ('plan')."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Description of the action or plan text to approve."
                    }
                },
                "required": ["flavor", "prompt"]
            }),
        )
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

            let flavor_str = match args["flavor"].as_str() {
                Some(f) => f,
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: flavor".into(),
                        is_error: true,
                    };
                }
            };
            let flavor = match flavor_str {
                "permission" => ApprovalFlavor::Permission,
                "plan" => ApprovalFlavor::Plan,
                other => {
                    return ToolOutcome::Immediate {
                        content: format!("Invalid flavor '{}'. Expected 'permission' or 'plan'.", other),
                        is_error: true,
                    };
                }
            };

            let prompt = match args["prompt"].as_str() {
                Some(p) => p.to_owned(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: prompt".into(),
                        is_error: true,
                    };
                }
            };

            let request_id = generate_request_id();
            let interaction = AgentInteraction::Approval {
                request_id: request_id.clone(),
                app_id: self.app_id.clone(),
                flavor,
                prompt,
            };

            self.recorder.record(
                TrajectoryEventDraft::new("tool.interaction.raised", "tool.interaction", &self.app_id)
                    .with_payload(serde_json::json!({
                        "tool": "request_approval",
                        "request_id": request_id,
                    })),
            );

            match enqueue_and_await(&self.queue, interaction).await {
                Ok(response) => {
                    let content = serde_json::to_string(&response).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e));
                    ToolOutcome::Immediate { content, is_error: false }
                }
                Err(msg) => ToolOutcome::Immediate { content: msg, is_error: true },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// AskQuestionTool
// ---------------------------------------------------------------------------

/// Tool that raises an [`AgentInteraction::Question`], enqueues it, and awaits
/// the user's answer. See `docs/tool/interaction.md`.
pub struct AskQuestionTool {
    queue: Arc<InteractionQueue>,
    recorder: SharedTrajectoryRecorder,
    app_id: String,
}

impl AskQuestionTool {
    pub fn new(queue: Arc<InteractionQueue>, recorder: SharedTrajectoryRecorder, app_id: impl Into<String>) -> Self {
        Self { queue, recorder, app_id: app_id.into() }
    }
}

impl Tool for AskQuestionTool {
    fn name(&self) -> &str {
        "ask_question"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "ask_question",
            "Ask the user an open question, optionally with suggested options. Blocks until the user answers.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The question text to show the user."
                    },
                    "options": {
                        "type": "array",
                        "description": "Optional list of suggested answers.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string", "description": "Short label." },
                                "description": { "type": "string", "description": "Longer explanation." }
                            },
                            "required": ["label", "description"]
                        }
                    }
                },
                "required": ["text"]
            }),
        )
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

            let text = match args["text"].as_str() {
                Some(t) => t.to_owned(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: text".into(),
                        is_error: true,
                    };
                }
            };

            let options = parse_choice_options(args["options"].as_array());

            let request_id = generate_request_id();
            let interaction = AgentInteraction::Question {
                request_id: request_id.clone(),
                app_id: self.app_id.clone(),
                text,
                options,
            };

            self.recorder.record(
                TrajectoryEventDraft::new("tool.interaction.raised", "tool.interaction", &self.app_id)
                    .with_payload(serde_json::json!({
                        "tool": "ask_question",
                        "request_id": request_id,
                    })),
            );

            match enqueue_and_await(&self.queue, interaction).await {
                Ok(response) => {
                    let content = serde_json::to_string(&response).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e));
                    ToolOutcome::Immediate { content, is_error: false }
                }
                Err(msg) => ToolOutcome::Immediate { content: msg, is_error: true },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// OfferChoiceTool
// ---------------------------------------------------------------------------

/// Tool that raises an [`AgentInteraction::Choice`], enqueues it, and awaits
/// the user's selection. See `docs/tool/interaction.md`.
pub struct OfferChoiceTool {
    queue: Arc<InteractionQueue>,
    recorder: SharedTrajectoryRecorder,
    app_id: String,
}

impl OfferChoiceTool {
    pub fn new(queue: Arc<InteractionQueue>, recorder: SharedTrajectoryRecorder, app_id: impl Into<String>) -> Self {
        Self { queue, recorder, app_id: app_id.into() }
    }
}

impl Tool for OfferChoiceTool {
    fn name(&self) -> &str {
        "offer_choice"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "offer_choice",
            "Force the user to select one of a set of mutually exclusive options. Blocks until the user selects.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The question or instruction shown above the options."
                    },
                    "options": {
                        "type": "array",
                        "description": "The mutually exclusive options to present.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string", "description": "Short label." },
                                "description": { "type": "string", "description": "Longer explanation." }
                            },
                            "required": ["label", "description"]
                        }
                    }
                },
                "required": ["prompt", "options"]
            }),
        )
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

            let prompt = match args["prompt"].as_str() {
                Some(p) => p.to_owned(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: prompt".into(),
                        is_error: true,
                    };
                }
            };

            let options = parse_choice_options(args["options"].as_array());

            let request_id = generate_request_id();
            let interaction = AgentInteraction::Choice {
                request_id: request_id.clone(),
                app_id: self.app_id.clone(),
                prompt,
                options,
            };

            self.recorder.record(
                TrajectoryEventDraft::new("tool.interaction.raised", "tool.interaction", &self.app_id)
                    .with_payload(serde_json::json!({
                        "tool": "offer_choice",
                        "request_id": request_id,
                    })),
            );

            match enqueue_and_await(&self.queue, interaction).await {
                Ok(response) => {
                    let content = serde_json::to_string(&response).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e));
                    ToolOutcome::Immediate { content, is_error: false }
                }
                Err(msg) => ToolOutcome::Immediate { content: msg, is_error: true },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// PresentPreviewTool
// ---------------------------------------------------------------------------

/// Tool that raises an [`AgentInteraction::Preview`], enqueues it, and awaits
/// the user's acknowledgement. See `docs/tool/interaction.md`.
pub struct PresentPreviewTool {
    queue: Arc<InteractionQueue>,
    recorder: SharedTrajectoryRecorder,
    app_id: String,
}

impl PresentPreviewTool {
    pub fn new(queue: Arc<InteractionQueue>, recorder: SharedTrajectoryRecorder, app_id: impl Into<String>) -> Self {
        Self { queue, recorder, app_id: app_id.into() }
    }
}

impl Tool for PresentPreviewTool {
    fn name(&self) -> &str {
        "present_preview"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "present_preview",
            "Present a generated artifact (image, markdown, etc.) to the user for acknowledgement. Blocks until the user acknowledges.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Context or instructions shown alongside the artifact."
                    },
                    "mime_type": {
                        "type": "string",
                        "description": "Media type of the artifact (e.g. 'text/markdown', 'image/png')."
                    },
                    "content": {
                        "type": "string",
                        "description": "Artifact content; encoding is determined by mime_type."
                    }
                },
                "required": ["prompt", "mime_type", "content"]
            }),
        )
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

            let prompt = match args["prompt"].as_str() {
                Some(p) => p.to_owned(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: prompt".into(),
                        is_error: true,
                    };
                }
            };

            let mime_type = match args["mime_type"].as_str() {
                Some(m) => m.to_owned(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: mime_type".into(),
                        is_error: true,
                    };
                }
            };

            let content = match args["content"].as_str() {
                Some(c) => c.to_owned(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: content".into(),
                        is_error: true,
                    };
                }
            };

            let request_id = generate_request_id();
            let interaction = AgentInteraction::Preview {
                request_id: request_id.clone(),
                app_id: self.app_id.clone(),
                prompt,
                artifact: PreviewArtifact { mime_type, content },
            };

            self.recorder.record(
                TrajectoryEventDraft::new("tool.interaction.raised", "tool.interaction", &self.app_id)
                    .with_payload(serde_json::json!({
                        "tool": "present_preview",
                        "request_id": request_id,
                    })),
            );

            match enqueue_and_await(&self.queue, interaction).await {
                Ok(response) => {
                    let content = serde_json::to_string(&response).unwrap_or_else(|e| format!("{{\"error\":\"{}\"}}", e));
                    ToolOutcome::Immediate { content, is_error: false }
                }
                Err(msg) => ToolOutcome::Immediate { content: msg, is_error: true },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Shared parsing helpers
// ---------------------------------------------------------------------------

fn parse_choice_options(array: Option<&Vec<serde_json::Value>>) -> Vec<ChoiceOption> {
    match array {
        None => Vec::new(),
        Some(arr) => arr
            .iter()
            .filter_map(|item| {
                let label = item["label"].as_str()?.to_owned();
                let description = item["description"].as_str()?.to_owned();
                Some(ChoiceOption { label, description })
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::external::interaction_queue::InteractionQueue;
    use crate::tool::Tool;
    use crate::trajectory::NoopTrajectoryRecorder;

    fn noop_recorder() -> SharedTrajectoryRecorder {
        Arc::new(NoopTrajectoryRecorder)
    }

    fn run<F: std::future::Future<Output = ToolOutcome>>(f: F) -> ToolOutcome {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    // ---------------------------------------------------------------------------
    // RequestApprovalTool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_request_approval_enqueues_and_resolves() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = RequestApprovalTool::new(queue.clone(), noop_recorder(), "app-test");

        // Execute the tool in a background task so we can resolve while it waits.
        let queue_clone = queue.clone();
        let handle = tokio::spawn(async move {
            tool.execute(r#"{"flavor":"permission","prompt":"delete file"}"#)
                .await
        });

        // Allow the tool to enqueue.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(queue_clone.pending_count(), 1, "should have one pending request");

        // Resolve the pending interaction.
        let request_ids: Vec<String> = queue_clone
            .pending_requests()
            .iter()
            .map(|r| crate::agent::external::get_request_id(r).to_owned())
            .collect();
        assert_eq!(request_ids.len(), 1);
        let resolved = queue_clone.resolve(
            &request_ids[0],
            crate::agent::external::UIInteractionResponse::PermissionGranted {
                request_id: request_ids[0].clone(),
            },
        );
        assert!(resolved, "resolve should return true");

        let outcome = handle.await.unwrap();
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error, "unexpected error: {}", content);
                // Response should contain approved kind.
                assert!(content.contains("approved"), "expected 'approved' in response, got: {}", content);
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[test]
    fn test_request_approval_missing_flavor() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = RequestApprovalTool::new(queue, noop_recorder(), "app-test");
        let outcome = run(tool.execute(r#"{"prompt":"do something"}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("flavor"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_request_approval_invalid_flavor() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = RequestApprovalTool::new(queue, noop_recorder(), "app-test");
        let outcome = run(tool.execute(r#"{"flavor":"unknown","prompt":"do something"}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Invalid flavor"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    #[test]
    fn test_request_approval_missing_prompt() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = RequestApprovalTool::new(queue, noop_recorder(), "app-test");
        let outcome = run(tool.execute(r#"{"flavor":"permission"}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("prompt"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    // ---------------------------------------------------------------------------
    // AskQuestionTool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_ask_question_enqueues_and_resolves() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = AskQuestionTool::new(queue.clone(), noop_recorder(), "app-test");

        let queue_clone = queue.clone();
        let handle = tokio::spawn(async move {
            tool.execute(r#"{"text":"which database?","options":[{"label":"pg","description":"postgres"}]}"#)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(queue_clone.pending_count(), 1);

        let request_ids: Vec<String> = queue_clone
            .pending_requests()
            .iter()
            .map(|r| crate::agent::external::get_request_id(r).to_owned())
            .collect();
        queue_clone.resolve(
            &request_ids[0],
            crate::agent::external::UIInteractionResponse::SelectedOption {
                request_id: request_ids[0].clone(),
                index: 0,
            },
        );

        let outcome = handle.await.unwrap();
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error, "unexpected error: {}", content);
                assert!(content.contains("answered"), "expected 'answered' in response, got: {}", content);
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[test]
    fn test_ask_question_missing_text() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = AskQuestionTool::new(queue, noop_recorder(), "app-test");
        let outcome = run(tool.execute(r#"{}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("text"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    // ---------------------------------------------------------------------------
    // OfferChoiceTool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_offer_choice_enqueues_and_resolves() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = OfferChoiceTool::new(queue.clone(), noop_recorder(), "app-test");

        let queue_clone = queue.clone();
        let handle = tokio::spawn(async move {
            tool.execute(r#"{"prompt":"pick theme","options":[{"label":"dark","description":"dark mode"},{"label":"light","description":"light mode"}]}"#)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(queue_clone.pending_count(), 1);

        let request_ids: Vec<String> = queue_clone
            .pending_requests()
            .iter()
            .map(|r| crate::agent::external::get_request_id(r).to_owned())
            .collect();
        queue_clone.resolve(
            &request_ids[0],
            crate::agent::external::UIInteractionResponse::SelectedOption {
                request_id: request_ids[0].clone(),
                index: 1,
            },
        );

        let outcome = handle.await.unwrap();
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error, "unexpected error: {}", content);
                assert!(content.contains("answered"), "expected 'answered' in response, got: {}", content);
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[test]
    fn test_offer_choice_missing_prompt() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = OfferChoiceTool::new(queue, noop_recorder(), "app-test");
        let outcome = run(tool.execute(r#"{"options":[]}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("prompt"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    // ---------------------------------------------------------------------------
    // PresentPreviewTool
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_present_preview_enqueues_and_resolves() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = PresentPreviewTool::new(queue.clone(), noop_recorder(), "app-test");

        let queue_clone = queue.clone();
        let handle = tokio::spawn(async move {
            tool.execute(r##"{"prompt":"review this","mime_type":"text/markdown","content":"# Hello"}"##)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(queue_clone.pending_count(), 1);

        let request_ids: Vec<String> = queue_clone
            .pending_requests()
            .iter()
            .map(|r| crate::agent::external::get_request_id(r).to_owned())
            .collect();
        queue_clone.resolve(
            &request_ids[0],
            crate::agent::external::UIInteractionResponse::PermissionGranted {
                request_id: request_ids[0].clone(),
            },
        );

        let outcome = handle.await.unwrap();
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error, "unexpected error: {}", content);
                // Preview acknowledgement bridges through PermissionGranted → Approved(Permission)
                assert!(content.contains("approved"), "expected 'approved' in response, got: {}", content);
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[test]
    fn test_present_preview_missing_mime_type() {
        let queue = Arc::new(InteractionQueue::new());
        let tool = PresentPreviewTool::new(queue, noop_recorder(), "app-test");
        let outcome = run(tool.execute(r#"{"prompt":"x","content":"y"}"#));
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("mime_type"));
            }
            _ => panic!("Expected Immediate"),
        }
    }

    // ---------------------------------------------------------------------------
    // Trajectory recording
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn test_request_approval_records_trajectory_event() {
        use crate::trajectory::MemoryTrajectoryRecorder;

        let queue = Arc::new(InteractionQueue::new());
        let recorder = Arc::new(MemoryTrajectoryRecorder::new());
        let shared: SharedTrajectoryRecorder = recorder.clone();
        let tool = RequestApprovalTool::new(queue.clone(), shared, "app-test");

        let queue_clone = queue.clone();
        let handle = tokio::spawn(async move {
            tool.execute(r#"{"flavor":"plan","prompt":"step 1; step 2"}"#)
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let request_ids: Vec<String> = queue_clone
            .pending_requests()
            .iter()
            .map(|r| crate::agent::external::get_request_id(r).to_owned())
            .collect();
        queue_clone.resolve(
            &request_ids[0],
            crate::agent::external::UIInteractionResponse::PlanApproved {
                request_id: request_ids[0].clone(),
            },
        );
        handle.await.unwrap();

        let events = recorder.events();
        assert!(
            events.iter().any(|e| e.event_type == "tool.interaction.raised"),
            "expected trajectory event, got: {:?}",
            events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
        );
    }

    // ---------------------------------------------------------------------------
    // parse_choice_options
    // ---------------------------------------------------------------------------

    #[test]
    fn test_parse_choice_options_empty() {
        let opts = parse_choice_options(None);
        assert!(opts.is_empty());
    }

    #[test]
    fn test_parse_choice_options_valid() {
        let arr = vec![
            serde_json::json!({"label": "a", "description": "alpha"}),
            serde_json::json!({"label": "b", "description": "beta"}),
        ];
        let opts = parse_choice_options(Some(&arr));
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "a");
        assert_eq!(opts[1].description, "beta");
    }
}
