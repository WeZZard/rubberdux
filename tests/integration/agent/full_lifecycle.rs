use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::runtime::subagent::{ContextEvent, spawn_subagent};
use rubberdux::hardened_prompts::subagent_preamble;
use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};
use rubberdux::tool::{SubagentType, ToolRegistry};

fn make_subagent_client(mock_uri: &str) -> Arc<MoonshotClient> {
    Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_uri.into(),
        "test-key".into(),
        "test-model".into(),
    ))
}

fn readonly_registry(client: &Arc<MoonshotClient>) -> Arc<ToolRegistry> {
    use rubberdux::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
    use rubberdux::provider::moonshot::tool::web_search::WebSearchTool;
    use rubberdux::tool::glob::GlobTool;
    use rubberdux::tool::grep::GrepTool;
    use rubberdux::tool::read::ReadFileTool;

    Arc::new({
        let mut r = ToolRegistry::new();
        r.register(Box::new(GlobTool));
        r.register(Box::new(GrepTool));
        r.register(Box::new(ReadFileTool));
        r.register(Box::new(MoonshotWebFetchTool::new()));
        r.register(Box::new(WebSearchTool::new(client.clone())));
        r
    })
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_context_broadcast_reaches_running_subagent() {
    let mock_server = MockServer::start().await;

    // Create a temp file so read_file succeeds
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "broadcast_test_data\n").unwrap();
    let file_path = tmp.path().to_str().unwrap();

    // Turn 1: delayed response so the broadcast has time to arrive
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(serde_json::json!({
                    "id": "cmpl-sub-1",
                    "object": "chat.completion",
                    "created": 1234567890,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Let me read the file.",
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_read_1",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": format!("{{\"file_path\":\"{}\"}}", file_path)
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

    // Turn 2: after tool result and broadcast, subagent stops
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-sub-2",
            "object": "chat.completion",
            "created": 1234567891,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Subagent finished"
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25 }
        })))
        .mount(&mock_server)
        .await;

    let client = make_subagent_client(&mock_server.uri());
    let registry = readonly_registry(&client);
    let preamble = subagent_preamble(SubagentType::Explore);
    let system_prompt = format!("{}", preamble);

    let (context_tx, _) = tokio::sync::broadcast::channel(8);
    let handle = spawn_subagent(
        "ctx_test".into(),
        client,
        system_prompt,
        "Initial prompt".into(),
        registry,
        context_tx.subscribe(),
        None,
        None,
    );

    // Broadcast a context update while the subagent is awaiting the delayed mock
    let broadcast_msg = Message::User {
        content: UserContent::Text("BROADCAST_42".into()),
    };
    let _ = context_tx.send(ContextEvent::UserMessage(broadcast_msg));

    // Wait for subagent to complete
    let result = tokio::time::timeout(Duration::from_secs(10), handle.result_rx)
        .await
        .expect("subagent should complete within 10s")
        .expect("result channel should not be dropped");

    assert_eq!(result.task_id, "ctx_test");

    // Inspect the request(s) sent to the mock server
    let requests = mock_server.received_requests().await.unwrap();
    assert!(!requests.is_empty(), "subagent should have called the LLM");

    let mut found_broadcast = false;
    for req in &requests {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        for msg in messages {
            if let Some(content) = msg["content"].as_str() {
                if content.contains("BROADCAST_42") {
                    found_broadcast = true;
                    break;
                }
            }
        }
    }

    assert!(
        found_broadcast,
        "broadcasted message should appear in at least one LLM request"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_concurrent_subagents_all_complete() {
    let mock_a = MockServer::start().await;
    let mock_b = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-a",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Result A" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .mount(&mock_a)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-b",
            "object": "chat.completion",
            "created": 1234567891,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Result B" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })))
        .mount(&mock_b)
        .await;

    let client_a = make_subagent_client(&mock_a.uri());
    let client_b = make_subagent_client(&mock_b.uri());
    let registry_a = readonly_registry(&client_a);
    let registry_b = readonly_registry(&client_b);

    let preamble = subagent_preamble(SubagentType::Explore);
    let system_prompt = format!("{}", preamble);

    let (context_tx_a, _) = tokio::sync::broadcast::channel(4);
    let (context_tx_b, _) = tokio::sync::broadcast::channel(4);

    let handle_a = spawn_subagent(
        "agent_a".into(),
        client_a,
        system_prompt.clone(),
        "Task A".into(),
        registry_a,
        context_tx_a.subscribe(),
        None,
        None,
    );

    let handle_b = spawn_subagent(
        "agent_b".into(),
        client_b,
        system_prompt,
        "Task B".into(),
        registry_b,
        context_tx_b.subscribe(),
        None,
        None,
    );

    let (result_a, result_b) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(10), handle_a.result_rx),
        tokio::time::timeout(Duration::from_secs(10), handle_b.result_rx),
    );

    let a = result_a
        .expect("agent_a should complete within 10s")
        .expect("result channel should not be dropped");
    let b = result_b
        .expect("agent_b should complete within 10s")
        .expect("result channel should not be dropped");

    assert_eq!(a.task_id, "agent_a");
    assert_eq!(a.summary, "Result A");
    assert_eq!(b.task_id, "agent_b");
    assert_eq!(b.summary, "Result B");
}
