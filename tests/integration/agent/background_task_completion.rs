use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use rubberdux::agent::runtime::compaction::EvictOldestTurns;
use rubberdux::agent::runtime::port::{LoopEvent, LoopOutput};
use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};

use crate::support::artifact;
use crate::support::mock_tools::{MockBackgroundTool, build_registry_with};

/// Test that background task completion reaches the AgentLoop and triggers
/// a final response with the correct reply_to metadata.
#[tokio::test]
async fn test_background_task_completion_reaches_agent_loop() {
    let mock_server = MockServer::start().await;

    // First response: assistant triggers a tool call
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-1",
            "object": "chat.completion",
            "created": 1,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "I'll run that in the background.",
                    "tool_calls": [{
                        "id": "call_bg1",
                        "type": "function",
                        "function": { "name": "bg_task", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second response: after background task completes
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-2",
                "object": "chat.completion",
                "created": 2,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "Background task completed with: result data" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25 }
            })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let mock_tool = MockBackgroundTool::new("bg_task");
    let registry = build_registry_with(vec![Box::new(mock_tool.clone())]);

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let artifact_dir = artifact::artifact_dir("test_background_task_completion");
    let session_path = artifact_dir.join("transcript.jsonl");

    let config = AgentLoopConfig {
        client,
        registry: Arc::new(registry),
        system_prompt: "You are a test assistant.".into(),
        session_path: Some(session_path.clone()),
        session_id: None,
        agent_id: Some("main".into()),
        recorder: None,
        tool_results_dir: Some(artifact_dir.join("tool-results")),
        token_budget: 100_000,
        cancel: CancellationToken::new(),
        compaction: Box::new(EvictOldestTurns),
        context_tx: None,
    };

    let (agent_loop, input_port) = AgentLoop::new(config).await;
    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await;
    });

    // Send message that triggers background task
    let (reply_tx, mut reply_rx) = mpsc::channel::<LoopOutput>(32);
    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("trigger background".into()),
        },
        reply: Some(reply_tx),
        metadata: Some(Box::new(42i32)),
    };
    input_port.send(event).await.unwrap();

    // Collect initial response (background task started)
    let mut initial_received = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(output)) = tokio::time::timeout_at(deadline, reply_rx.recv()).await {
        if !output.is_final {
            initial_received = true;
            break;
        }
    }
    assert!(
        initial_received,
        "Should receive non-final initial response"
    );

    // Complete background task
    mock_tool.complete("result data");

    // Collect final response
    let mut final_received = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(output)) = tokio::time::timeout_at(deadline, reply_rx.recv()).await {
        if output.is_final {
            final_received = true;
            // Verify reply_to metadata is preserved
            let metadata = output
                .metadata
                .as_ref()
                .and_then(|m| m.downcast_ref::<i32>().copied());
            assert_eq!(metadata, Some(42), "reply_to metadata should be preserved");
            break;
        }
    }
    assert!(
        final_received,
        "Should receive final response after background task completes"
    );

    let events_path = artifact_dir.join("events.jsonl");
    let events = wait_for_events(&events_path).await;
    assert!(
        events.contains("\"type\":\"agent.started\""),
        "events.jsonl should contain agent lifecycle events: {}",
        events
    );
    assert!(
        events.contains("\"type\":\"model.requested\""),
        "events.jsonl should contain model lifecycle events: {}",
        events
    );
    for line in events.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("trajectory event should be valid JSON");
    }

    agent_handle.abort();
}

async fn wait_for_events(events_path: &PathBuf) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(content) = tokio::fs::read_to_string(events_path).await {
            if content.contains("model.requested") {
                return content;
            }
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {}",
            events_path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
