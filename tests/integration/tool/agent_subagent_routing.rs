use std::sync::Arc;

use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::agent::runtime::subagent::spawn_subagent;
use rubberdux::hardened_prompts::subagent_preamble;
use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::tool::agent::{AgentTool, build_subagent_registries};
use rubberdux::tool::{SubagentType, ToolRegistry};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup() -> (Arc<MoonshotClient>, ToolRegistry) {
    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        "http://localhost:0".into(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registries = build_subagent_registries(&client);
    let (context_tx, _) = broadcast::channel(4);

    let agent_tool = AgentTool::new(
        client.clone(),
        registries,
        "integration test system prompt".into(),
        context_tx,
        None,
        None,
    );

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(agent_tool));
    (client, registry)
}

fn tool_call_response(tool_name: &str, tool_args: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cmpl-turn1",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Let me check.",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_001",
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": tool_args
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 50, "completion_tokens": 20, "total_tokens": 70 }
    })
}

fn stop_response(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cmpl-turn2",
        "choices": [{
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 80, "completion_tokens": 15, "total_tokens": 95 }
    })
}

/// Extract tool names from the first request's `tools` array.
fn tool_names_from_request(body: &serde_json::Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Schema test
// ---------------------------------------------------------------------------

#[test]
fn test_agent_tool_definition_has_subagent_type() {
    let (_client, registry) = setup();
    let defs = registry.definitions();
    let agent_def = defs.iter().find(|d| d.function.name == "agent").unwrap();

    let params = agent_def.function.parameters.as_ref().unwrap();
    let required = params["required"].as_array().unwrap();
    let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        required_names.contains(&"subagent_type"),
        "subagent_type must be in required, got {:?}",
        required_names
    );

    let enum_values = params["properties"]["subagent_type"]["enum"]
        .as_array()
        .unwrap();
    let enum_strs: Vec<&str> = enum_values.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(enum_strs.len(), 4);
    assert!(enum_strs.contains(&"explore"));
    assert!(enum_strs.contains(&"plan"));
    assert!(enum_strs.contains(&"general_purpose"));
    assert!(enum_strs.contains(&"computer_use"));
}

// ---------------------------------------------------------------------------
// Multi-turn happy-path tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_explore_subagent_happy_path() {
    let mock_server = MockServer::start().await;

    // Turn 1: LLM asks to glob for .rs files
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response(
            "glob",
            r#"{"pattern":"*.rs","path":"src/tool"}"#,
        )))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 2: LLM produces final answer
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_response("Found tool source files")),
        )
        .mount(&mock_server)
        .await;

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registries = build_subagent_registries(&client);
    let registry = registries.get(&SubagentType::Explore).unwrap().clone();

    let preamble = subagent_preamble(SubagentType::Explore);
    let system_prompt = format!("{}\n\nBase system prompt.", preamble);

    let (context_tx, _) = broadcast::channel(4);
    let handle = spawn_subagent(
        "explore_happy".into(),
        client,
        system_prompt,
        "Find all tool source files".into(),
        registry,
        context_tx.subscribe(),
        None,
        None,
    );

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle.result_rx)
        .await
        .expect("subagent should complete within 10s")
        .expect("result channel should not be dropped");

    assert_eq!(result.task_id, "explore_happy");
    assert!(
        result.summary.contains("Found tool source files"),
        "unexpected summary: {}",
        result.summary
    );

    // Inspect captured requests
    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected 2 LLM turns");

    // Turn 1: verify preamble and tool definitions
    let turn1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let system_content = turn1["messages"][0]["content"].as_str().unwrap();
    assert!(
        system_content.contains(preamble),
        "system prompt should contain Explore preamble"
    );

    let tools = tool_names_from_request(&turn1);
    for expected in ["glob", "grep", "read_file", "web_fetch", "web_search"] {
        assert!(
            tools.contains(&expected.to_owned()),
            "Explore tools should include {}",
            expected
        );
    }
    for absent in ["bash", "write_file", "edit_file", "agent"] {
        assert!(
            !tools.contains(&absent.to_owned()),
            "Explore tools should NOT include {}",
            absent
        );
    }

    // Turn 2: verify tool result message was included
    let turn2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = turn2["messages"].as_array().unwrap();
    let has_tool_result = messages
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_001");
    assert!(
        has_tool_result,
        "turn 2 should include tool result for glob"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_plan_subagent_happy_path() {
    let mock_server = MockServer::start().await;

    // Turn 1: LLM asks to read Cargo.toml
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_call_response(
            "read_file",
            r#"{"file_path":"Cargo.toml"}"#,
        )))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 2: stop
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_response("Analyzed project structure")),
        )
        .mount(&mock_server)
        .await;

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registries = build_subagent_registries(&client);
    let registry = registries.get(&SubagentType::Plan).unwrap().clone();

    let preamble = subagent_preamble(SubagentType::Plan);
    let system_prompt = format!("{}\n\nBase system prompt.", preamble);

    let (context_tx, _) = broadcast::channel(4);
    let handle = spawn_subagent(
        "plan_happy".into(),
        client,
        system_prompt,
        "Analyze project dependencies".into(),
        registry,
        context_tx.subscribe(),
        None,
        None,
    );

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle.result_rx)
        .await
        .expect("subagent should complete within 10s")
        .expect("result channel should not be dropped");

    assert_eq!(result.task_id, "plan_happy");
    assert!(
        result.summary.contains("Analyzed project structure"),
        "unexpected summary: {}",
        result.summary
    );

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected 2 LLM turns");

    // Turn 1: verify preamble and read-only tools
    let turn1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let system_content = turn1["messages"][0]["content"].as_str().unwrap();
    assert!(
        system_content.contains(preamble),
        "system prompt should contain Plan preamble"
    );

    let tools = tool_names_from_request(&turn1);
    for expected in ["glob", "grep", "read_file", "web_fetch", "web_search"] {
        assert!(
            tools.contains(&expected.to_owned()),
            "Plan tools should include {}",
            expected
        );
    }
    for absent in ["bash", "write_file", "edit_file"] {
        assert!(
            !tools.contains(&absent.to_owned()),
            "Plan tools should NOT include {}",
            absent
        );
    }

    // Turn 2: verify tool result message
    let turn2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = turn2["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call_001");
    assert!(
        tool_msg.is_some(),
        "turn 2 should include tool result for read_file"
    );

    // The tool result should contain actual Cargo.toml content
    let tool_content = tool_msg.unwrap()["content"].as_str().unwrap();
    assert!(
        tool_content.contains("rubberdux"),
        "read_file result should contain package name, got: {}",
        &tool_content[..tool_content.len().min(200)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gp_subagent_happy_path() {
    let mock_server = MockServer::start().await;

    // Turn 1: LLM asks to run a bash command (GP-only tool)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(tool_call_response("bash", r#"{"command":"echo hello"}"#)),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 2: stop
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_response("Command executed")))
        .mount(&mock_server)
        .await;

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registries = build_subagent_registries(&client);
    let registry = registries
        .get(&SubagentType::GeneralPurpose)
        .unwrap()
        .clone();

    let preamble = subagent_preamble(SubagentType::GeneralPurpose);
    let system_prompt = format!("{}\n\nBase system prompt.", preamble);

    let (context_tx, _) = broadcast::channel(4);
    let handle = spawn_subagent(
        "gp_happy".into(),
        client,
        system_prompt,
        "Run echo hello".into(),
        registry,
        context_tx.subscribe(),
        None,
        None,
    );

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle.result_rx)
        .await
        .expect("subagent should complete within 10s")
        .expect("result channel should not be dropped");

    assert_eq!(result.task_id, "gp_happy");
    assert!(
        result.summary.contains("Command executed"),
        "unexpected summary: {}",
        result.summary
    );

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected 2 LLM turns");

    // Turn 1: verify GP preamble and GP-specific tools
    let turn1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let system_content = turn1["messages"][0]["content"].as_str().unwrap();
    assert!(
        system_content.contains(preamble),
        "system prompt should contain GP preamble"
    );

    let tools = tool_names_from_request(&turn1);
    for gp_only in ["bash", "write_file", "edit_file"] {
        assert!(
            tools.contains(&gp_only.to_owned()),
            "GP tools should include {} (GP-only tool)",
            gp_only
        );
    }
    assert!(
        !tools.contains(&"agent".to_owned()),
        "GP tools should NOT include agent"
    );

    // Turn 2: verify bash result was passed back
    let turn2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = turn2["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call_001");
    assert!(
        tool_msg.is_some(),
        "turn 2 should include tool result for bash"
    );

    let tool_content = tool_msg.unwrap()["content"].as_str().unwrap();
    assert!(
        tool_content.contains("hello"),
        "bash result should contain 'hello', got: {}",
        tool_content
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_computer_use_subagent_happy_path() {
    let mock_server = MockServer::start().await;

    // Turn 1: LLM asks to run a bash command (ComputerUse has bash)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(tool_call_response("bash", r#"{"command":"echo hello"}"#)),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Turn 2: stop
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_response("Command executed")))
        .mount(&mock_server)
        .await;

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registries = build_subagent_registries(&client);
    let registry = registries.get(&SubagentType::ComputerUse).unwrap().clone();

    let preamble = subagent_preamble(SubagentType::ComputerUse);
    let system_prompt = format!("{}\n\nBase system prompt.", preamble);

    let (context_tx, _) = broadcast::channel(4);
    let handle = spawn_subagent(
        "cu_happy".into(),
        client,
        system_prompt,
        "Run echo hello".into(),
        registry,
        context_tx.subscribe(),
        None,
        None,
    );

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle.result_rx)
        .await
        .expect("subagent should complete within 10s")
        .expect("result channel should not be dropped");

    assert_eq!(result.task_id, "cu_happy");
    assert!(
        result.summary.contains("Command executed"),
        "unexpected summary: {}",
        result.summary
    );

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected 2 LLM turns");

    // Turn 1: verify ComputerUse preamble and tools
    let turn1: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let system_content = turn1["messages"][0]["content"].as_str().unwrap();
    assert!(
        system_content.contains(preamble),
        "system prompt should contain ComputerUse preamble"
    );

    let tools = tool_names_from_request(&turn1);
    for expected in ["bash", "write_file", "edit_file"] {
        assert!(
            tools.contains(&expected.to_owned()),
            "ComputerUse tools should include {}",
            expected
        );
    }
    assert!(
        !tools.contains(&"agent".to_owned()),
        "ComputerUse tools should NOT include agent"
    );

    // Turn 2: verify bash result was passed back
    let turn2: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = turn2["messages"].as_array().unwrap();
    let tool_msg = messages
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "call_001");
    assert!(
        tool_msg.is_some(),
        "turn 2 should include tool result for bash"
    );

    let tool_content = tool_msg.unwrap()["content"].as_str().unwrap();
    assert!(
        tool_content.contains("hello"),
        "bash result should contain 'hello', got: {}",
        tool_content
    );
}
