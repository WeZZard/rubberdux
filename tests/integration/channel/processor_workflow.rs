use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::entry::EntryOrigin;
use rubberdux::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use rubberdux::agent::runtime::compaction::EvictOldestTurns;
use rubberdux::agent::runtime::port::{EntryNotification, LoopEvent};
use rubberdux::channel::processor::ChannelProcessor;
use rubberdux::provider::kimi_for_coding::{Message, UserContent};
use crate::support::model_api_stub::openai_model_api;
use rubberdux::tool::ToolRegistry;

use crate::support::artifact;
use crate::support::mock_channel_processor::MockChannelProcessor;

/// Mount a single mock LLM response that returns plain text.
async fn mount_plain_text_response(mock_server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-chan-1",
            "object": "chat.completion",
            "created": 1,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from the assistant."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .mount(mock_server)
        .await;
}

/// Build a minimal AgentLoopConfig wired to the mock server and the given
/// channel processors map.
fn build_config(
    mock_server_uri: &str,
    test_name: &str,
    channel_processors: HashMap<String, Arc<dyn ChannelProcessor>>,
) -> (AgentLoopConfig, std::path::PathBuf) {
    let client = openai_model_api(mock_server_uri, "test-model");

    let artifact_dir = artifact::artifact_dir(test_name);
    let session_path = artifact_dir.join("transcript.jsonl");

    let config = AgentLoopConfig {
        client,
        registry: Arc::new(ToolRegistry::new()),
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
        channel_processors,
        guardrails: rubberdux::guardrail::GuardrailChain::new(),
    };

    (config, artifact_dir)
}

/// Collect broadcast notifications until a final assistant entry arrives or
/// the deadline expires.
async fn collect_until_final(
    entry_rx: &mut tokio::sync::broadcast::Receiver<EntryNotification>,
    timeout: Duration,
) -> Vec<EntryNotification> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, entry_rx.recv()).await {
            Ok(Ok(notification)) => {
                let is_final = notification.is_final;
                collected.push(notification);
                if is_final {
                    break;
                }
            }
            _ => break,
        }
    }
    collected
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The channel processor matching the user message origin is invoked and
/// receives the assistant entry.
#[tokio::test]
async fn test_channel_processor_called_for_matching_origin() {
    let mock_server = MockServer::start().await;
    mount_plain_text_response(&mock_server).await;

    let processor = Arc::new(MockChannelProcessor::new("telegram"));

    let mut processors: HashMap<String, Arc<dyn ChannelProcessor>> = HashMap::new();
    processors.insert("telegram".into(), processor.clone());

    let (config, _artifact_dir) = build_config(
        &mock_server.uri(),
        "test_channel_processor_called_for_matching_origin",
        processors,
    );

    let (agent_loop, input_port) = AgentLoop::new(config).await;
    let mut entry_rx = agent_loop.subscribe_output().into_receiver();
    let handle = tokio::spawn(async move { agent_loop.run().await });

    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("hi".into()),
        },
        origin: EntryOrigin::User {
            channel: "telegram".into(),
        },
        channel_metadata: Some(serde_json::json!({"telegram_chat_id": 123})),
    };
    input_port.send(event).await.unwrap();

    let notifications = collect_until_final(&mut entry_rx, Duration::from_secs(5)).await;
    assert!(
        notifications.iter().any(|n| n.is_final),
        "Should receive a final assistant response"
    );

    assert_eq!(
        processor.call_count(),
        1,
        "Processor should be called exactly once"
    );

    let received = processor.entries_received().await;
    assert!(
        !received.is_empty(),
        "Processor should have received at least one entry ID"
    );

    // The received entry ID should match one of the assistant notifications.
    let assistant_ids: Vec<usize> = notifications
        .iter()
        .filter(|n| matches!(&n.entry.message, Message::Assistant { .. }))
        .map(|n| n.entry.id)
        .collect();
    assert!(
        received.iter().any(|id| assistant_ids.contains(id)),
        "Processor should have received an assistant entry ID, got {:?} vs {:?}",
        received,
        assistant_ids
    );

    handle.abort();
}

/// A channel processor registered for a different channel name is not called.
#[tokio::test]
async fn test_channel_processor_not_called_for_different_origin() {
    let mock_server = MockServer::start().await;
    mount_plain_text_response(&mock_server).await;

    let processor = Arc::new(MockChannelProcessor::new("telegram"));

    let mut processors: HashMap<String, Arc<dyn ChannelProcessor>> = HashMap::new();
    processors.insert("telegram".into(), processor.clone());

    let (config, _artifact_dir) = build_config(
        &mock_server.uri(),
        "test_channel_processor_not_called_for_different_origin",
        processors,
    );

    let (agent_loop, input_port) = AgentLoop::new(config).await;
    let mut entry_rx = agent_loop.subscribe_output().into_receiver();
    let handle = tokio::spawn(async move { agent_loop.run().await });

    // Send with "gateway" origin — no matching processor.
    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("hi".into()),
        },
        origin: EntryOrigin::User {
            channel: "gateway".into(),
        },
        channel_metadata: None,
    };
    input_port.send(event).await.unwrap();

    let notifications = collect_until_final(&mut entry_rx, Duration::from_secs(5)).await;
    assert!(
        notifications.iter().any(|n| n.is_final),
        "Should still receive a final assistant response"
    );

    assert_eq!(
        processor.call_count(),
        0,
        "Processor should not be called for a non-matching channel"
    );

    handle.abort();
}

/// A failing channel processor does not prevent the response from being
/// broadcast. The assistant entry should still arrive with `is_final == true`.
#[tokio::test]
async fn test_channel_processor_error_does_not_block() {
    let mock_server = MockServer::start().await;
    mount_plain_text_response(&mock_server).await;

    let processor = Arc::new(MockChannelProcessor::failing("telegram"));

    let mut processors: HashMap<String, Arc<dyn ChannelProcessor>> = HashMap::new();
    processors.insert("telegram".into(), processor.clone());

    let (config, _artifact_dir) = build_config(
        &mock_server.uri(),
        "test_channel_processor_error_does_not_block",
        processors,
    );

    let (agent_loop, input_port) = AgentLoop::new(config).await;
    let mut entry_rx = agent_loop.subscribe_output().into_receiver();
    let handle = tokio::spawn(async move { agent_loop.run().await });

    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("hi".into()),
        },
        origin: EntryOrigin::User {
            channel: "telegram".into(),
        },
        channel_metadata: Some(serde_json::json!({"telegram_chat_id": 456})),
    };
    input_port.send(event).await.unwrap();

    let notifications = collect_until_final(&mut entry_rx, Duration::from_secs(5)).await;

    // Response arrives despite processor error.
    assert!(
        notifications.iter().any(|n| n.is_final),
        "Final response should still arrive even when processor fails"
    );

    // Processor was invoked (and failed).
    assert_eq!(
        processor.call_count(),
        1,
        "Failing processor should still be called once"
    );

    // The entry was broadcast (we received notifications).
    let assistant_count = notifications
        .iter()
        .filter(|n| matches!(&n.entry.message, Message::Assistant { .. }))
        .count();
    assert!(
        assistant_count >= 1,
        "Assistant entry should be broadcast despite processor error"
    );

    handle.abort();
}
