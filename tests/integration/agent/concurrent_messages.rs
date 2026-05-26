use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::entry::EntryOrigin;
use rubberdux::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use rubberdux::agent::runtime::compaction::EvictOldestTurns;
use rubberdux::agent::runtime::port::{EntryNotification, LoopEvent};
use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};
use rubberdux::tool::ToolRegistry;

use crate::support::artifact;
use crate::support::log_capture;

/// Helper: set up an AgentLoop pointing to an already-running mock server.
/// Creates artifact directory, captures logs, and persists transcripts.
async fn setup_agent_loop(
    mock_uri: &str,
    test_name: &str,
) -> (
    rubberdux::agent::runtime::port::InputPort,
    broadcast::Receiver<EntryNotification>,
    tokio::task::JoinHandle<()>,
    PathBuf, // session_path
) {
    let artifact_dir = artifact::artifact_dir(test_name);
    let session_path = artifact_dir.join("transcript.jsonl");
    let log_path = artifact_dir.join("test.log");

    log_capture::init(&log_path);
    log::info!(
        "Test {} starting. Artifacts in {:?}",
        test_name,
        artifact_dir
    );

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_uri.into(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registry = Arc::new(ToolRegistry::new());

    let config = AgentLoopConfig {
        client,
        registry,
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
    let entry_rx = agent_loop.subscribe_output().into_receiver();
    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await;
    });

    (input_port, entry_rx, agent_handle, session_path)
}

/// Helper: send a message and collect responses until is_final.
async fn send_and_collect(
    input_port: &rubberdux::agent::runtime::port::InputPort,
    entry_rx: &mut broadcast::Receiver<EntryNotification>,
    text: &str,
) -> String {
    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text(text.into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: None,
    };
    input_port.send(event).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, entry_rx.recv()).await {
            Ok(Ok(notification)) => {
                if let Message::Assistant { content, .. } = &notification.entry.message {
                    if notification.is_final {
                        return content.clone().unwrap_or_default();
                    }
                }
            }
            Ok(Err(_)) => panic!("Broadcast channel closed without final response"),
            Err(_) => panic!("Timed out waiting for response"),
        }
    }
}

/// Helper: read transcript entries from session file.
fn read_transcript(session_path: &PathBuf) -> Vec<rubberdux::agent::entry::Entry> {
    let content = std::fs::read_to_string(session_path).unwrap_or_default();
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Two messages
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_agent_loop_handles_two_messages() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(serde_json::json!({
                    "id": "cmpl-1",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "Response for message 1" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-2",
            "object": "chat.completion",
            "created": 2,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Response for message 2" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let (input_port, mut entry_rx, agent_handle, session_path) =
        setup_agent_loop(&mock_server.uri(), "test_agent_loop_handles_two_messages").await;

    // Send both messages in quick succession
    let out1 = send_and_collect(&input_port, &mut entry_rx, "Message 1").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out2 = send_and_collect(&input_port, &mut entry_rx, "Message 2").await;

    // Generate and write narration
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // Verify responses
    assert!(
        out1.contains("Response for message 1"),
        "msg1: {}",
        out1
    );
    assert!(
        out2.contains("Response for message 2"),
        "msg2: {}",
        out2
    );

    // Verify transcript
    let entries = read_transcript(&session_path);
    log::info!("Transcript has {} entries", entries.len());
    assert!(
        entries.len() >= 3,
        "Expected at least 3 entries (system + 2 user + 2 assistant), got {}",
        entries.len()
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// Three messages
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_agent_loop_handles_three_messages() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(400))
                .set_body_json(serde_json::json!({
                    "id": "cmpl-1",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "Response for message 1" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(300))
                .set_body_json(serde_json::json!({
                    "id": "cmpl-2",
                    "object": "chat.completion",
                    "created": 2,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "Response for message 2" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-3",
            "object": "chat.completion",
            "created": 3,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Response for message 3" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let (input_port, mut entry_rx, agent_handle, session_path) =
        setup_agent_loop(&mock_server.uri(), "test_agent_loop_handles_three_messages").await;

    let out1 = send_and_collect(&input_port, &mut entry_rx, "Message 1").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out2 = send_and_collect(&input_port, &mut entry_rx, "Message 2").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out3 = send_and_collect(&input_port, &mut entry_rx, "Message 3").await;

    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    assert!(
        out1.contains("Response for message 1"),
        "msg1: {}",
        out1
    );
    assert!(
        out2.contains("Response for message 2"),
        "msg2: {}",
        out2
    );
    assert!(
        out3.contains("Response for message 3"),
        "msg3: {}",
        out3
    );

    let entries = read_transcript(&session_path);
    log::info!("Transcript has {} entries", entries.len());
    assert!(
        entries.len() >= 4,
        "Expected at least 4 entries, got {}",
        entries.len()
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// Four messages
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_agent_loop_handles_four_messages() {
    let mock_server = MockServer::start().await;

    for i in 1..=4 {
        let delay = if i < 3 {
            Duration::from_millis(300)
        } else {
            Duration::from_millis(0)
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(
                serde_json::json!({
                    "id": format!("cmpl-{}", i),
                    "object": "chat.completion",
                    "created": i,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": format!("Response for message {}", i)
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                }),
            ))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
    }

    let (input_port, mut entry_rx, agent_handle, session_path) =
        setup_agent_loop(&mock_server.uri(), "test_agent_loop_handles_four_messages").await;

    let out1 = send_and_collect(&input_port, &mut entry_rx, "Message 1").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out2 = send_and_collect(&input_port, &mut entry_rx, "Message 2").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out3 = send_and_collect(&input_port, &mut entry_rx, "Message 3").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out4 = send_and_collect(&input_port, &mut entry_rx, "Message 4").await;

    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    assert!(
        out1.contains("Response for message 1"),
        "msg1: {}",
        out1
    );
    assert!(
        out2.contains("Response for message 2"),
        "msg2: {}",
        out2
    );
    assert!(
        out3.contains("Response for message 3"),
        "msg3: {}",
        out3
    );
    assert!(
        out4.contains("Response for message 4"),
        "msg4: {}",
        out4
    );

    let entries = read_transcript(&session_path);
    log::info!("Transcript has {} entries", entries.len());
    assert!(
        entries.len() >= 5,
        "Expected at least 5 entries, got {}",
        entries.len()
    );

    agent_handle.abort();
}

/// Regression test: When a message triggers a tool call (non-final response),
/// and a second message arrives while the first is processing, both messages
/// must receive responses.
///
/// This reproduces the production bug where concurrent messages caused issues
/// with response routing.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_concurrent_messages_with_tool_call_preserve_reply_to() {
    let mock_server = MockServer::start().await;

    // First call: tool call response (non-final) with 200ms delay
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(serde_json::json!({
                    "id": "cmpl-tool",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Let me check the news.",
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "web_search",
                                    "arguments": "{\"query\": \"latest Google news\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: final response after tool execution
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-final",
            "object": "chat.completion",
            "created": 2,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Here are the latest Google news updates."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Third call: response for message 2
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-msg2",
            "object": "chat.completion",
            "created": 3,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "I am running on Ubuntu 24.04."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let (input_port, mut entry_rx, agent_handle, session_path) = setup_agent_loop(
        &mock_server.uri(),
        "test_concurrent_messages_with_tool_call_preserve_reply_to",
    )
    .await;

    // Send message 1 (will trigger tool call)
    let event1 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Search for latest Google news".into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: Some(serde_json::json!(100)),
    };
    input_port.send(event1).await.unwrap();

    // Immediately send message 2 while tool call is processing
    tokio::time::sleep(Duration::from_millis(50)).await;
    let event2 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("What OS are you running?".into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: Some(serde_json::json!(200)),
    };
    input_port.send(event2).await.unwrap();

    // Collect all final assistant responses
    let mut final_responses = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, entry_rx.recv()).await {
            Ok(Ok(notification)) => {
                if let Message::Assistant { content, .. } = &notification.entry.message {
                    if notification.is_final {
                        final_responses.push(content.clone().unwrap_or_default());
                        if final_responses.len() >= 2 {
                            break;
                        }
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    // Generate narration for debugging
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // Verify we got both final responses
    assert!(
        final_responses.len() >= 2,
        "Should receive at least 2 final responses, got {}",
        final_responses.len()
    );

    // Verify transcript contains all entries
    let entries = read_transcript(&session_path);
    log::info!("Transcript has {} entries", entries.len());
    assert!(
        entries.len() >= 5,
        "Expected at least 5 entries (system + 2 user + 2 assistant + tool results), got {}",
        entries.len()
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// Regression test: Concurrent messages must not swap response content
// ---------------------------------------------------------------------------

/// Reproduction for the production bug where two concurrent messages get
/// responses with swapped content.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_concurrent_messages_do_not_swap_content() {
    let mock_server = MockServer::start().await;

    // First call: slow response for message 1 (500ms delay)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(serde_json::json!({
                    "id": "cmpl-1",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Response for message 1: spawn agent"
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: fast response for message 2
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
                    "content": "Response for message 2: environment"
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    let (input_port, mut entry_rx, agent_handle, session_path) = setup_agent_loop(
        &mock_server.uri(),
        "test_concurrent_messages_do_not_swap_content",
    )
    .await;

    // Send message 1 (slow)
    let event1 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Message 1: spawn agent".into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: Some(serde_json::json!(100)),
    };
    input_port.send(event1).await.unwrap();

    // Send message 2 immediately while message 1 is still processing
    tokio::time::sleep(Duration::from_millis(50)).await;
    let event2 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Message 2: environment".into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: Some(serde_json::json!(200)),
    };
    input_port.send(event2).await.unwrap();

    // Collect all final assistant responses (the agent processes sequentially)
    let mut final_texts = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, entry_rx.recv()).await {
            Ok(Ok(notification)) => {
                if let Message::Assistant { content, .. } = &notification.entry.message {
                    if notification.is_final {
                        final_texts.push(content.clone().unwrap_or_default());
                        if final_texts.len() >= 2 {
                            break;
                        }
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    // Generate narration for debugging
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // Verify both responses arrived (agent processes them sequentially)
    assert_eq!(
        final_texts.len(),
        2,
        "Expected exactly 2 final responses, got {}",
        final_texts.len()
    );

    // Verify message 1 got its response first, message 2 second
    assert!(
        final_texts[0].contains("Response for message 1"),
        "First response should be for message 1, got: {}",
        final_texts[0]
    );
    assert!(
        final_texts[1].contains("Response for message 2"),
        "Second response should be for message 2, got: {}",
        final_texts[1]
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// Regression test: Error responses must be observable
// ---------------------------------------------------------------------------

/// Reproduction for the production bug where LLM API errors cause
/// responses to be lost.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_error_response_preserves_reply_to_metadata() {
    let mock_server = MockServer::start().await;

    // Mock returns HTTP 400 to simulate an LLM API error
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": "invalid temperature",
                "type": "invalid_request_error"
            }
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let (input_port, mut entry_rx, agent_handle, session_path) = setup_agent_loop(
        &mock_server.uri(),
        "test_error_response_preserves_reply_to_metadata",
    )
    .await;

    // Send a message with channel_metadata
    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Hello".into()),
        },
        origin: EntryOrigin::User {
            channel: "test".into(),
        },
        channel_metadata: Some(serde_json::json!(42)),
    };
    input_port.send(event).await.unwrap();

    // Collect the error response
    let mut error_text = String::new();
    let mut received_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, entry_rx.recv()).await {
            Ok(Ok(notification)) => {
                if let Message::Assistant { content, .. } = &notification.entry.message {
                    error_text = content.clone().unwrap_or_default();
                    if notification.is_final {
                        received_final = true;
                        break;
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    // Generate artifacts for debugging
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // The response should contain the error text
    assert!(
        error_text.contains("invalid temperature"),
        "Expected error text in response, got: {}",
        error_text
    );
    assert!(received_final, "Error response should be final");

    agent_handle.abort();
}
