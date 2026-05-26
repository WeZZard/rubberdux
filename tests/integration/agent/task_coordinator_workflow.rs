use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::entry::EntryOrigin;
use rubberdux::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use rubberdux::agent::runtime::compaction::EvictOldestTurns;
use rubberdux::agent::runtime::port::{EntryNotification, LoopEvent};
use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};

use crate::support::artifact;
use crate::support::mock_tools::{MockTool, build_registry_with};

/// Test that tool result entries are persisted with sequential IDs and
/// broadcast through the entry notification channel.
///
/// Setup:
/// - First LLM response triggers 2 tool calls (immediate MockTool).
/// - Second LLM response is plain text with finish_reason "stop".
///
/// Asserts:
/// - Broadcast entries include tool result entries (role "tool").
/// - Tool entries have sequential IDs.
/// - session.jsonl contains the tool entries.
#[tokio::test]
async fn test_tool_entry_ids_persisted_and_broadcast() {
    let mock_server = MockServer::start().await;

    // First response: assistant triggers two tool calls
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
                    "content": "Running both tools now.",
                    "tool_calls": [
                        {
                            "id": "call_tool_a",
                            "type": "function",
                            "function": { "name": "tool_a", "arguments": "{}" }
                        },
                        {
                            "id": "call_tool_b",
                            "type": "function",
                            "function": { "name": "tool_b", "arguments": "{}" }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second response: plain text, stop
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-2",
            "object": "chat.completion",
            "created": 2,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Both tools finished successfully."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let tool_a = MockTool::new("tool_a", Duration::ZERO);
    let tool_b = MockTool::new("tool_b", Duration::ZERO);
    let registry = build_registry_with(vec![Box::new(tool_a), Box::new(tool_b)]);

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let artifact_dir = artifact::artifact_dir("test_tool_entry_ids_persisted_and_broadcast");
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
        channel_processors: std::collections::HashMap::new(),
        guardrails: rubberdux::guardrail::GuardrailChain::new(),
    };

    let (agent_loop, input_port) = AgentLoop::new(config).await;
    let mut entry_rx = agent_loop.subscribe_output().into_receiver();
    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await;
    });

    // Send user message that triggers the two tool calls
    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("run both tools".into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: None,
    };
    input_port.send(event).await.unwrap();

    // Collect all broadcast notifications until final
    let mut notifications: Vec<EntryNotification> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, entry_rx.recv()).await {
            Ok(Ok(notification)) => {
                let is_final = notification.is_final;
                notifications.push(notification);
                if is_final {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("Timed out waiting for final notification"),
        }
    }

    // Filter tool entries from broadcast
    let tool_notifications: Vec<&EntryNotification> = notifications
        .iter()
        .filter(|n| matches!(&n.entry.message, Message::Tool { .. }))
        .collect();

    assert_eq!(
        tool_notifications.len(),
        2,
        "Expected 2 tool result entries in broadcast, got {}",
        tool_notifications.len()
    );

    // Verify tool entries have sequential IDs
    let tool_id_0 = tool_notifications[0].entry.id;
    let tool_id_1 = tool_notifications[1].entry.id;
    assert_eq!(
        tool_id_1,
        tool_id_0 + 1,
        "Tool entry IDs should be sequential: got {} and {}",
        tool_id_0,
        tool_id_1
    );

    // Verify tool_call_ids match what we sent
    let tool_call_ids: Vec<&str> = tool_notifications
        .iter()
        .map(|n| match &n.entry.message {
            Message::Tool { tool_call_id, .. } => tool_call_id.as_str(),
            _ => unreachable!(),
        })
        .collect();
    assert!(
        tool_call_ids.contains(&"call_tool_a"),
        "Should contain tool result for call_tool_a"
    );
    assert!(
        tool_call_ids.contains(&"call_tool_b"),
        "Should contain tool result for call_tool_b"
    );

    // Wait briefly for session file to be flushed
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify session.jsonl contains the tool entries
    let session_content = tokio::fs::read_to_string(&session_path)
        .await
        .expect("session.jsonl should exist");

    let session_entries: Vec<serde_json::Value> = session_content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each line should be valid JSON"))
        .collect();

    let persisted_tool_entries: Vec<&serde_json::Value> = session_entries
        .iter()
        .filter(|e| e["message"]["role"].as_str() == Some("tool"))
        .collect();

    assert_eq!(
        persisted_tool_entries.len(),
        2,
        "Expected 2 tool entries in session.jsonl, got {}",
        persisted_tool_entries.len()
    );

    // Verify persisted entries have IDs matching broadcast
    let persisted_ids: Vec<u64> = persisted_tool_entries
        .iter()
        .map(|e| e["id"].as_u64().expect("entry should have numeric id"))
        .collect();
    assert!(
        persisted_ids.contains(&(tool_id_0 as u64)),
        "session.jsonl should contain tool entry with id {}",
        tool_id_0
    );
    assert!(
        persisted_ids.contains(&(tool_id_1 as u64)),
        "session.jsonl should contain tool entry with id {}",
        tool_id_1
    );

    agent_handle.abort();
}
