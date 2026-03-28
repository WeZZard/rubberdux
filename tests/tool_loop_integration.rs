use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::provider::moonshot::{Message, UserContent};
use rubberdux::tool;

/// Test: simple text response (no tool calls)
#[tokio::test]
async fn test_simple_text_response() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-test",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "total_tokens": 18
            }
        })))
        .mount(&mock_server)
        .await;

    let client = MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    );

    let messages = vec![
        Message::System {
            content: "You are helpful.".into(),
        },
        Message::User {
            content: UserContent::Text("Hello".into()),
        },
    ];

    let response = client.chat(messages, None).await.unwrap();

    assert_eq!(response.choices[0].finish_reason, "stop");
    assert_eq!(
        response.choices[0].message.content_text(),
        "Hello! How can I help you?"
    );
    assert_eq!(response.usage.prompt_tokens, 10);
}

/// Test: tool call response followed by tool result, then final text
#[tokio::test]
async fn test_tool_call_loop() {
    let mock_server = MockServer::start().await;

    // First call: model requests a tool call
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "cmpl-1",
                    "object": "chat.completion",
                    "created": 1234567890,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Let me check that for you.",
                            "tool_calls": [{
                                "index": 0,
                                "id": "tool_abc123",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": "{\"file_path\": \"/etc/hostname\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {
                        "prompt_tokens": 20,
                        "completion_tokens": 15,
                        "total_tokens": 35
                    }
                }))
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: after tool result, model gives final response
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "id": "cmpl-2",
                    "object": "chat.completion",
                    "created": 1234567891,
                    "model": "test-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "The hostname is: test-machine"
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": 40,
                        "completion_tokens": 10,
                        "total_tokens": 50
                    }
                }))
        )
        .mount(&mock_server)
        .await;

    let client = MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    );

    let tools = tool::load_tool_definitions(std::path::Path::new("./tools"));

    let mut history: Vec<Message> = vec![
        Message::User {
            content: UserContent::Text("Read /etc/hostname".into()),
        },
    ];

    // Simulate the tool use loop
    let mut final_text = String::new();

    loop {
        let mut messages = vec![Message::System {
            content: "You are helpful.".into(),
        }];
        messages.extend_from_slice(&history);

        let response = client.chat(messages, Some(tools.clone())).await.unwrap();
        let choice = &response.choices[0];

        let text = choice.message.content_text();
        if !text.is_empty() {
            final_text = text.to_owned();
        }

        history.push(choice.message.clone());

        if choice.finish_reason == "stop" {
            break;
        }

        // Execute tool calls
        if let Some(tool_calls) = choice.message.tool_calls() {
            for call in tool_calls {
                let result =
                    tool::execute_tool(&call.function.name, &call.function.arguments).await;

                history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: result.content,
                });
            }
        }
    }

    assert_eq!(final_text, "The hostname is: test-machine");
    // History should contain: user, assistant(tool_call), tool_result, assistant(final)
    assert_eq!(history.len(), 4);
    assert!(matches!(&history[0], Message::User { .. }));
    assert!(matches!(&history[1], Message::Assistant { .. }));
    assert!(matches!(&history[2], Message::Tool { .. }));
    assert!(matches!(&history[3], Message::Assistant { .. }));
}

/// Test: tool execution produces correct output
#[tokio::test]
async fn test_read_file_tool_execution() {
    // Create a temp file to read
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "line1\nline2\nline3\n").unwrap();

    let args = serde_json::json!({
        "file_path": tmp.path().to_str().unwrap()
    });

    let result = tool::execute_tool("read_file", &serde_json::to_string(&args).unwrap()).await;

    assert!(!result.is_error);
    assert!(result.content.contains("line1"));
    assert!(result.content.contains("line2"));
    assert!(result.content.contains("line3"));
}

/// Test: bash tool sync execution
#[tokio::test]
async fn test_bash_tool_sync() {
    let args = serde_json::json!({
        "command": "echo hello_test_output"
    });

    let result = tool::execute_tool("bash", &serde_json::to_string(&args).unwrap()).await;

    assert!(!result.is_error);
    assert!(result.content.contains("hello_test_output"));
}

/// Test: bash tool background execution
#[tokio::test]
async fn test_bash_tool_background() {
    let args = serde_json::json!({
        "command": "echo bg_test",
        "run_in_background": true
    });

    let result = tool::execute_tool("bash", &serde_json::to_string(&args).unwrap()).await;

    assert!(!result.is_error);
    assert!(result.content.contains("Command running in background"));
    assert!(result.content.contains("Output is being written to"));

    // Wait briefly for background task to complete
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Extract output path and verify file was written
    let output_path = result
        .content
        .split("Output is being written to: ")
        .nth(1)
        .unwrap()
        .trim();

    let output = std::fs::read_to_string(output_path).unwrap();
    assert!(output.contains("bg_test"));

    // Cleanup
    let _ = std::fs::remove_file(output_path);
}

/// Test: edit tool str_replace
#[tokio::test]
async fn test_edit_file_tool() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "hello world\nfoo bar\n").unwrap();

    let args = serde_json::json!({
        "file_path": tmp.path().to_str().unwrap(),
        "old_string": "foo bar",
        "new_string": "baz qux"
    });

    let result = tool::execute_tool("edit_file", &serde_json::to_string(&args).unwrap()).await;

    assert!(!result.is_error);

    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert!(content.contains("baz qux"));
    assert!(!content.contains("foo bar"));
}

/// Test: session JSONL roundtrip
#[tokio::test]
async fn test_session_jsonl_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path();

    let messages = vec![
        Message::User {
            content: UserContent::Text("Hello".into()),
        },
        Message::Assistant {
            content: Some("Hi there!".into()),
            reasoning_content: Some("User said hello, respond friendly".into()),
            tool_calls: None,
            partial: None,
        },
    ];

    // Write
    for msg in &messages {
        let json = serde_json::to_string(msg).unwrap();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{}", json).unwrap();
    }

    // Read back
    let content = std::fs::read_to_string(path).unwrap();
    let mut restored: Vec<Message> = Vec::new();
    for line in content.lines() {
        if !line.trim().is_empty() {
            restored.push(serde_json::from_str(line).unwrap());
        }
    }

    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].content_text(), "Hello");
    assert_eq!(restored[1].content_text(), "Hi there!");

    // Verify reasoning_content survives roundtrip
    if let Message::Assistant {
        reasoning_content, ..
    } = &restored[1]
    {
        assert_eq!(
            reasoning_content.as_deref(),
            Some("User said hello, respond friendly")
        );
    } else {
        panic!("Expected Assistant message");
    }
}

/// Test: telegram adapter response parsing
#[test]
fn test_extract_reply_from_model_output() {
    let output = r#"Let me think about this.
<telegram-message from="assistant" to="user">Hello! Here is your answer.</telegram-message>
Some internal reasoning here."#;

    let tag = "<telegram-message from=\"assistant\" to=\"user\">";
    let end_tag = "</telegram-message>";
    let start = output.find(tag).unwrap();
    let content_start = start + tag.len();
    let end = output[content_start..].find(end_tag).unwrap();
    let reply = &output[content_start..content_start + end];

    assert_eq!(reply, "Hello! Here is your answer.");
}

/// Test: telegram adapter reaction parsing
#[test]
fn test_extract_reactions_from_model_output() {
    let output = r#"<telegram-reaction from="assistant" action="add" emoji="👍" message-id="42" />
<telegram-message from="assistant" to="user">Great question!</telegram-message>"#;

    let tag_prefix = "<telegram-reaction from=\"assistant\"";
    assert!(output.contains(tag_prefix));

    // Extract emoji
    let emoji_start = output.find("emoji=\"").unwrap() + "emoji=\"".len();
    let emoji_end = output[emoji_start..].find('"').unwrap();
    let emoji = &output[emoji_start..emoji_start + emoji_end];
    assert_eq!(emoji, "👍");

    // Extract message-id
    let mid_start = output.find("message-id=\"").unwrap() + "message-id=\"".len();
    let mid_end = output[mid_start..].find('"').unwrap();
    let mid: i32 = output[mid_start..mid_start + mid_end].parse().unwrap();
    assert_eq!(mid, 42);
}
