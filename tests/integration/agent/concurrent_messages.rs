use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use rubberdux::agent::runtime::compaction::EvictOldestTurns;
use rubberdux::agent::runtime::port::{LoopEvent, LoopOutput};
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
    };

    let (agent_loop, input_port) = AgentLoop::new(config).await;
    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await;
    });

    (input_port, agent_handle, session_path)
}

/// Helper: send a message and collect exactly one response.
async fn send_and_collect(
    input_port: &rubberdux::agent::runtime::port::InputPort,
    text: &str,
    metadata_id: i32,
) -> LoopOutput {
    let (reply_tx, mut reply_rx) = mpsc::channel::<LoopOutput>(8);

    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text(text.into()),
        },
        reply: Some(reply_tx),
        metadata: Some(Box::new(metadata_id)),
    };
    input_port.send(event).await.unwrap();

    tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
        .await
        .expect("Timed out waiting for response")
        .expect("Reply channel closed without response")
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

    let (input_port, agent_handle, session_path) =
        setup_agent_loop(&mock_server.uri(), "test_agent_loop_handles_two_messages").await;

    // Send both messages in quick succession
    let out1 = send_and_collect(&input_port, "Message 1", 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out2 = send_and_collect(&input_port, "Message 2", 2).await;

    // Generate and write narration
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // Verify responses
    assert!(
        out1.text.contains("Response for message 1"),
        "msg1: {}",
        out1.text
    );
    assert!(
        out2.text.contains("Response for message 2"),
        "msg2: {}",
        out2.text
    );

    assert_eq!(
        out1.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(1)
    );
    assert_eq!(
        out2.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(2)
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

    let (input_port, agent_handle, session_path) =
        setup_agent_loop(&mock_server.uri(), "test_agent_loop_handles_three_messages").await;

    let out1 = send_and_collect(&input_port, "Message 1", 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out2 = send_and_collect(&input_port, "Message 2", 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out3 = send_and_collect(&input_port, "Message 3", 3).await;

    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    assert!(
        out1.text.contains("Response for message 1"),
        "msg1: {}",
        out1.text
    );
    assert!(
        out2.text.contains("Response for message 2"),
        "msg2: {}",
        out2.text
    );
    assert!(
        out3.text.contains("Response for message 3"),
        "msg3: {}",
        out3.text
    );

    assert_eq!(
        out1.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(1)
    );
    assert_eq!(
        out2.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(2)
    );
    assert_eq!(
        out3.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(3)
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

    let (input_port, agent_handle, session_path) =
        setup_agent_loop(&mock_server.uri(), "test_agent_loop_handles_four_messages").await;

    let out1 = send_and_collect(&input_port, "Message 1", 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out2 = send_and_collect(&input_port, "Message 2", 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out3 = send_and_collect(&input_port, "Message 3", 3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out4 = send_and_collect(&input_port, "Message 4", 4).await;

    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    assert!(
        out1.text.contains("Response for message 1"),
        "msg1: {}",
        out1.text
    );
    assert!(
        out2.text.contains("Response for message 2"),
        "msg2: {}",
        out2.text
    );
    assert!(
        out3.text.contains("Response for message 3"),
        "msg3: {}",
        out3.text
    );
    assert!(
        out4.text.contains("Response for message 4"),
        "msg4: {}",
        out4.text
    );

    assert_eq!(
        out1.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(1)
    );
    assert_eq!(
        out2.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(2)
    );
    assert_eq!(
        out3.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(3)
    );
    assert_eq!(
        out4.metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(4)
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
/// must receive responses with correct reply_to metadata.
///
/// This reproduces the production bug where reply_to_message_id becomes None
/// for responses sent after a tool call.
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

    let (input_port, agent_handle, session_path) = setup_agent_loop(
        &mock_server.uri(),
        "test_concurrent_messages_with_tool_call_preserve_reply_to",
    )
    .await;

    // Send message 1 (will trigger tool call)
    let (reply_tx1, mut reply_rx1) = mpsc::channel::<LoopOutput>(8);
    let event1 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Search for latest Google news".into()),
        },
        reply: Some(reply_tx1),
        metadata: Some(Box::new(100i32)),
    };
    input_port.send(event1).await.unwrap();

    // Immediately send message 2 while tool call is processing
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (reply_tx2, mut reply_rx2) = mpsc::channel::<LoopOutput>(8);
    let event2 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("What OS are you running?".into()),
        },
        reply: Some(reply_tx2),
        metadata: Some(Box::new(200i32)),
    };
    input_port.send(event2).await.unwrap();

    // Collect all responses for message 1
    let mut msg1_outputs = Vec::new();
    let timeout = Duration::from_secs(5);
    loop {
        match tokio::time::timeout(timeout, reply_rx1.recv()).await {
            Ok(Some(output)) => {
                let is_final = output.is_final;
                msg1_outputs.push(output);
                if is_final {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => panic!("Timed out waiting for msg1 responses"),
        }
    }

    // Collect response for message 2
    let msg2_output = tokio::time::timeout(timeout, reply_rx2.recv())
        .await
        .expect("Timed out waiting for msg2 response")
        .expect("Msg2 channel closed");

    // Generate narration for debugging
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // Verify message 1 got responses
    assert!(
        !msg1_outputs.is_empty(),
        "Message 1 should receive at least one response"
    );

    // Verify all msg1 responses preserve metadata=100
    for (i, output) in msg1_outputs.iter().enumerate() {
        let metadata = output
            .metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied());
        assert_eq!(
            metadata,
            Some(100),
            "Msg1 response {} should preserve metadata=100, got {:?}. Text: {}",
            i,
            metadata,
            output.text
        );
    }

    // Verify message 2 response preserves metadata=200
    let msg2_metadata = msg2_output
        .metadata
        .as_ref()
        .and_then(|m| m.downcast_ref::<i32>().copied());
    assert_eq!(
        msg2_metadata,
        Some(200),
        "Msg2 response should preserve metadata=200, got {:?}. Text: {}",
        msg2_metadata,
        msg2_output.text
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
/// responses with swapped content — the response for message 2 is sent to
/// message 1's reply channel and vice versa.
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

    let (input_port, agent_handle, session_path) = setup_agent_loop(
        &mock_server.uri(),
        "test_concurrent_messages_do_not_swap_content",
    )
    .await;

    // Send message 1 (slow)
    let (reply_tx1, mut reply_rx1) = mpsc::channel::<LoopOutput>(8);
    let event1 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Message 1: spawn agent".into()),
        },
        reply: Some(reply_tx1),
        metadata: Some(Box::new(100i32)),
    };
    input_port.send(event1).await.unwrap();

    // Send message 2 immediately while message 1 is still processing
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (reply_tx2, mut reply_rx2) = mpsc::channel::<LoopOutput>(8);
    let event2 = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Message 2: environment".into()),
        },
        reply: Some(reply_tx2),
        metadata: Some(Box::new(200i32)),
    };
    input_port.send(event2).await.unwrap();

    // Collect responses
    let timeout = Duration::from_secs(5);
    let msg1_output = tokio::time::timeout(timeout, reply_rx1.recv())
        .await
        .expect("Timed out waiting for msg1")
        .expect("Msg1 channel closed");

    let msg2_output = tokio::time::timeout(timeout, reply_rx2.recv())
        .await
        .expect("Timed out waiting for msg2")
        .expect("Msg2 channel closed");

    // Generate narration for debugging
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // Verify message 1 got the correct content
    assert!(
        msg1_output
            .text
            .contains("Response for message 1: spawn agent"),
        "Message 1 should receive response for message 1, got: {}",
        msg1_output.text
    );
    assert_eq!(
        msg1_output
            .metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(100),
        "Message 1 should have metadata=100"
    );

    // Verify message 2 got the correct content
    assert!(
        msg2_output
            .text
            .contains("Response for message 2: environment"),
        "Message 2 should receive response for message 2, got: {}",
        msg2_output.text
    );
    assert_eq!(
        msg2_output
            .metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied()),
        Some(200),
        "Message 2 should have metadata=200"
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// Regression test: Error responses must preserve reply_to metadata
// ---------------------------------------------------------------------------

/// Reproduction for the production bug where LLM API errors cause
/// reply_to_message_id to be lost, making the host drop the response.
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

    let (input_port, agent_handle, session_path) = setup_agent_loop(
        &mock_server.uri(),
        "test_error_response_preserves_reply_to_metadata",
    )
    .await;

    // Send a message with metadata simulating telegram_message_id=42
    let (reply_tx, mut reply_rx) = mpsc::channel::<LoopOutput>(8);
    let event = LoopEvent::UserMessage {
        message: Message::User {
            content: UserContent::Text("Hello".into()),
        },
        reply: Some(reply_tx),
        metadata: Some(Box::new(42i32)),
    };
    input_port.send(event).await.unwrap();

    // Collect the error response
    let output = tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
        .await
        .expect("Timed out waiting for error response")
        .expect("Reply channel closed without response");

    // Generate artifacts for debugging
    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    // The response should contain the error text
    assert!(
        output.text.contains("invalid temperature"),
        "Expected error text in response, got: {}",
        output.text
    );
    assert!(output.is_final, "Error response should be final");

    // CRITICAL: Metadata must be preserved so the host can route the response
    let metadata = output
        .metadata
        .as_ref()
        .and_then(|m| m.downcast_ref::<i32>().copied());
    assert_eq!(
        metadata,
        Some(42),
        "Error response must preserve reply_to metadata (telegram_message_id). \
         Got {:?} instead of Some(42). This reproduces the production bug \
         where LLM errors cause reply_to=None and the host drops the response.",
        metadata
    );

    agent_handle.abort();
}
