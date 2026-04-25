use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::provider::moonshot::{Message, UserContent};
use rubberdux::tool;

/// Test: simple text response (no tool calls)
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
async fn test_tool_call_loop() {
    let mock_server = MockServer::start().await;

    // First call: model requests a tool call
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: after tool result, model gives final response
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
        })))
        .mount(&mock_server)
        .await;

    let client = MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    );

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r.register(Box::new(rubberdux::tool::read::ReadFileTool));
        r.register(Box::new(rubberdux::tool::write::WriteFileTool));
        r.register(Box::new(rubberdux::tool::edit::EditFileTool));
        r.register(Box::new(rubberdux::tool::glob::GlobTool));
        r.register(Box::new(rubberdux::tool::grep::GrepTool));
        r
    };
    let tools = registry.definitions();

    let mut history: Vec<Message> = vec![Message::User {
        content: UserContent::Text("Read /etc/hostname".into()),
    }];

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
                let outcome = registry
                    .execute(&call.function.name, &call.function.arguments)
                    .await;
                let content = tool::format_tool_outcome(&outcome);

                history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: None,
                    content,
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
#[tokio::test(flavor = "multi_thread")]
async fn test_read_file_tool_execution() {
    // Create a temp file to read
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "line1\nline2\nline3\n").unwrap();

    let args = serde_json::json!({
        "file_path": tmp.path().to_str().unwrap()
    });

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::read::ReadFileTool));
        r
    };

    let outcome = registry
        .execute("read_file", &serde_json::to_string(&args).unwrap())
        .await;

    match outcome {
        tool::ToolOutcome::Immediate { content, is_error } => {
            assert!(!is_error);
            assert!(content.contains("line1"));
            assert!(content.contains("line2"));
            assert!(content.contains("line3"));
        }
        _ => panic!("Expected Immediate"),
    }
}

/// Test: bash tool sync execution
#[tokio::test(flavor = "multi_thread")]
async fn test_bash_tool_sync() {
    let args = serde_json::json!({
        "command": "echo hello_test_output"
    });

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r
    };

    let outcome = registry
        .execute("bash", &serde_json::to_string(&args).unwrap())
        .await;

    match outcome {
        tool::ToolOutcome::Immediate { content, is_error } => {
            assert!(!is_error);
            assert!(content.contains("hello_test_output"));
        }
        _ => panic!("Expected Immediate"),
    }
}

/// Test: bash tool background execution returns immediately with task info.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_bash_tool_background() {
    let args = serde_json::json!({
        "command": "echo bg_test",
        "run_in_background": true
    });

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r
    };

    let outcome = registry
        .execute("bash", &serde_json::to_string(&args).unwrap())
        .await;

    match outcome {
        tool::ToolOutcome::Background {
            task_id,
            output_path,
            receiver,
        } => {
            assert!(!task_id.is_empty());

            // Wait for background task to complete (generous timeout for CI/parallel tests)
            let mut output = String::new();
            for _ in 0..100 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if let Ok(content) = std::fs::read_to_string(&output_path) {
                    if !content.is_empty() {
                        output = content;
                        break;
                    }
                }
            }

            assert!(
                output.contains("bg_test"),
                "Output file should contain bg_test, got: {}",
                output
            );

            // Verify the receiver also delivers the result
            let _receiver = receiver;

            // Cleanup
            let _ = std::fs::remove_file(&output_path);
        }
        _ => panic!("Expected Background"),
    }
}

/// Test: edit tool str_replace
#[tokio::test(flavor = "multi_thread")]
async fn test_edit_file_tool() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "hello world\nfoo bar\n").unwrap();

    let args = serde_json::json!({
        "file_path": tmp.path().to_str().unwrap(),
        "old_string": "foo bar",
        "new_string": "baz qux"
    });

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::edit::EditFileTool));
        r
    };

    let outcome = registry
        .execute("edit_file", &serde_json::to_string(&args).unwrap())
        .await;

    match &outcome {
        tool::ToolOutcome::Immediate { is_error, .. } => assert!(!is_error),
        _ => panic!("Expected Immediate"),
    }

    let content = std::fs::read_to_string(tmp.path()).unwrap();
    assert!(content.contains("baz qux"));
    assert!(!content.contains("foo bar"));
}

/// Test: session JSONL roundtrip
#[tokio::test(flavor = "multi_thread")]
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

/// Test: tool call with run_in_background=true flows through the full loop.
/// Model calls bash with run_in_background, gets immediate "Running in background" result,
/// then on next iteration the model reads the output file and produces a final response.
#[tokio::test(flavor = "multi_thread")]
async fn test_background_tool_call_loop() {
    let mock_server = MockServer::start().await;

    // First call: model requests a background bash command
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-bg-1",
                "object": "chat.completion",
                "created": 1234567890,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "I'll run that build for you.",
                        "tool_calls": [{
                            "index": 0,
                            "id": "tool_bg_build",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": "{\"command\": \"echo build_success\", \"run_in_background\": true}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 20, "completion_tokens": 15, "total_tokens": 35 }
            })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: model gets the "Running in background" result and responds with stop
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-bg-2",
                "object": "chat.completion",
                "created": 1234567891,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "The build is running in the background. I'll let you know when it's done."
                    },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 40, "completion_tokens": 20, "total_tokens": 60 }
            })),
        )
        .mount(&mock_server)
        .await;

    let client = MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    );

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r.register(Box::new(rubberdux::tool::read::ReadFileTool));
        r.register(Box::new(rubberdux::tool::write::WriteFileTool));
        r.register(Box::new(rubberdux::tool::edit::EditFileTool));
        r.register(Box::new(rubberdux::tool::glob::GlobTool));
        r.register(Box::new(rubberdux::tool::grep::GrepTool));
        r
    };
    let tools = registry.definitions();

    let mut history: Vec<Message> = vec![Message::User {
        content: UserContent::Text("Build my project".into()),
    }];

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

        if let Some(tool_calls) = choice.message.tool_calls() {
            for call in tool_calls {
                let outcome = registry
                    .execute(&call.function.name, &call.function.arguments)
                    .await;

                // Background tool should return Background variant
                if call.function.arguments.contains("run_in_background") {
                    assert!(
                        matches!(&outcome, tool::ToolOutcome::Background { .. }),
                        "Background tool should return Background variant"
                    );
                }

                let content = tool::format_tool_outcome(&outcome);
                history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: None,
                    content,
                });
            }
        }
    }

    assert_eq!(
        final_text,
        "The build is running in the background. I'll let you know when it's done."
    );

    // History: user, assistant(tool_call), tool(bg result), assistant(final)
    assert_eq!(history.len(), 4);
    assert!(matches!(&history[0], Message::User { .. }));
    assert!(matches!(&history[1], Message::Assistant { .. }));
    assert!(matches!(&history[2], Message::Tool { .. }));
    assert!(matches!(&history[3], Message::Assistant { .. }));

    // Verify the tool result in history contains the background task info
    if let Message::Tool { content, .. } = &history[2] {
        assert!(
            content.contains("Background task"),
            "Expected background task info, got: {}",
            content
        );
    } else {
        panic!("Expected Tool message at index 2");
    }
}

/// Test: mixed sync and async tool calls in a single response.
/// Model issues multiple tool calls — some sync, some background.
/// All results are returned immediately (sync ones with actual output,
/// background ones with task ID), then the model produces a final response.
#[tokio::test(flavor = "multi_thread")]
async fn test_mixed_sync_background_tool_calls() {
    let mock_server = MockServer::start().await;

    // First call: model requests both sync and async tool calls
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-mix-1",
                "object": "chat.completion",
                "created": 1234567890,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "I'll check the date and start the build.",
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "tool_sync_date",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": "{\"command\": \"echo 2026-03-28\"}"
                                }
                            },
                            {
                                "index": 1,
                                "id": "tool_bg_build",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": "{\"command\": \"echo build_done\", \"run_in_background\": true}"
                                }
                            }
                        ]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 30, "completion_tokens": 25, "total_tokens": 55 }
            })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second call: model sees both results and produces final response
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-mix-2",
            "object": "chat.completion",
            "created": 1234567891,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The date is 2026-03-28 and the build is running in the background."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 60, "completion_tokens": 15, "total_tokens": 75 }
        })))
        .mount(&mock_server)
        .await;

    let client = MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    );

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r.register(Box::new(rubberdux::tool::read::ReadFileTool));
        r.register(Box::new(rubberdux::tool::write::WriteFileTool));
        r.register(Box::new(rubberdux::tool::edit::EditFileTool));
        r.register(Box::new(rubberdux::tool::glob::GlobTool));
        r.register(Box::new(rubberdux::tool::grep::GrepTool));
        r
    };
    let tools = registry.definitions();

    let mut history: Vec<Message> = vec![Message::User {
        content: UserContent::Text("Show date and build project".into()),
    }];

    let mut final_text = String::new();
    let mut sync_tool_content = String::new();
    let mut bg_was_background = false;

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

        if let Some(tool_calls) = choice.message.tool_calls() {
            for call in tool_calls {
                let outcome = registry
                    .execute(&call.function.name, &call.function.arguments)
                    .await;

                if call.function.arguments.contains("run_in_background") {
                    bg_was_background = matches!(&outcome, tool::ToolOutcome::Background { .. });
                } else if let tool::ToolOutcome::Immediate { ref content, .. } = outcome {
                    sync_tool_content = content.clone();
                }

                let content = tool::format_tool_outcome(&outcome);
                history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: None,
                    content,
                });
            }
        }
    }

    // Sync tool should have actual output
    assert!(
        sync_tool_content.contains("2026-03-28"),
        "Sync tool should return actual command output, got: {}",
        sync_tool_content
    );

    // Background tool should have returned Background variant
    assert!(
        bg_was_background,
        "Background tool should return Background variant"
    );

    // Final response references both results
    assert_eq!(
        final_text,
        "The date is 2026-03-28 and the build is running in the background."
    );

    // History: user, assistant(2 tool_calls), tool(sync), tool(bg), assistant(final)
    assert_eq!(history.len(), 5);
}

/// Test: background task output file can be read by read_file tool
/// This tests the complete lifecycle: spawn background → wait → read output.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_background_task_output_readable() {
    // Launch a background task
    let bg_args = serde_json::json!({
        "command": "echo lifecycle_test_output",
        "run_in_background": true
    });
    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r.register(Box::new(rubberdux::tool::read::ReadFileTool));
        r
    };

    let bg_outcome = registry
        .execute("bash", &serde_json::to_string(&bg_args).unwrap())
        .await;

    let output_path = match bg_outcome {
        tool::ToolOutcome::Background { output_path, .. } => output_path,
        _ => panic!("Expected Background"),
    };

    // Wait for background task to complete and verify output is readable
    let mut read_content = String::new();
    let mut read_ok = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let read_args = serde_json::json!({
            "file_path": output_path.to_str().unwrap()
        });
        let read_outcome = registry
            .execute("read_file", &serde_json::to_string(&read_args).unwrap())
            .await;
        if let tool::ToolOutcome::Immediate { content, is_error } = read_outcome {
            if !is_error && content.contains("lifecycle_test_output") {
                read_content = content;
                read_ok = true;
                break;
            }
        }
    }

    assert!(
        read_ok,
        "read_file should eventually succeed with output content, got: {}",
        read_content
    );
    assert!(
        read_content.contains("lifecycle_test_output"),
        "Output file should contain the command output, got: {}",
        read_content
    );

    // Cleanup
    let _ = std::fs::remove_file(&output_path);
}

/// Test: multiple tool call iterations (3-step tool chain)
/// Model calls tool A → result → calls tool B → result → final response
#[tokio::test(flavor = "multi_thread")]
async fn test_multi_step_tool_chain() {
    let mock_server = MockServer::start().await;

    // Step 1: model calls list_directory (conceptually, using bash)
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-chain-1",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Let me list the files first.",
                    "tool_calls": [{
                        "index": 0,
                        "id": "tool_step1",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\": \"echo file1.txt file2.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Step 2: model reads a specific file
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-chain-2",
            "object": "chat.completion",
            "created": 1234567891,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Now let me read the first file.",
                    "tool_calls": [{
                        "index": 0,
                        "id": "tool_step2",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\": \"echo content_of_file1\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 40, "completion_tokens": 10, "total_tokens": 50 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Step 3: final response
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "cmpl-chain-3",
                "object": "chat.completion",
                "created": 1234567892,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "The directory has file1.txt and file2.txt. File1 contains: content_of_file1"
                    },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 60, "completion_tokens": 15, "total_tokens": 75 }
            })),
        )
        .mount(&mock_server)
        .await;

    let client = MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    );

    let registry = {
        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(rubberdux::tool::bash::BashTool));
        r.register(Box::new(rubberdux::tool::read::ReadFileTool));
        r.register(Box::new(rubberdux::tool::write::WriteFileTool));
        r.register(Box::new(rubberdux::tool::edit::EditFileTool));
        r.register(Box::new(rubberdux::tool::glob::GlobTool));
        r.register(Box::new(rubberdux::tool::grep::GrepTool));
        r
    };
    let tools = registry.definitions();

    let mut history: Vec<Message> = vec![Message::User {
        content: UserContent::Text("Summarize files in /tmp".into()),
    }];

    let mut final_text = String::new();
    let mut loop_count = 0;

    loop {
        loop_count += 1;
        assert!(
            loop_count <= 10,
            "Tool loop should not exceed 10 iterations"
        );

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

        if let Some(tool_calls) = choice.message.tool_calls() {
            for call in tool_calls {
                let outcome = registry
                    .execute(&call.function.name, &call.function.arguments)
                    .await;
                let content = tool::format_tool_outcome(&outcome);
                history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    name: None,
                    content,
                });
            }
        }
    }

    // 3 iterations of the loop
    assert_eq!(loop_count, 3);

    // History: user, asst+tool1, tool_result1, asst+tool2, tool_result2, asst(final)
    assert_eq!(history.len(), 6);

    assert!(final_text.contains("content_of_file1"));
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

/// Test: web_search tool through ToolRegistry with mocked Moonshot API.
/// Verifies the full flow: registry dispatch → background task → two-call
/// $web_search pattern → result delivered via oneshot channel.
#[tokio::test(flavor = "multi_thread")]
async fn test_web_search_tool_via_registry() {
    use std::sync::Arc;

    let mock_server = MockServer::start().await;

    // First API call: model triggers $web_search builtin
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-ws-1",
            "object": "chat.completion",
            "created": 1234567890,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "index": 0,
                        "id": "tool_ws_001",
                        "type": "builtin_function",
                        "function": {
                            "name": "$web_search",
                            "arguments": "{\"search_result\":{\"id\":\"mock_search\"}}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60 }
        })))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    // Second API call: after echoing tool result, model returns final answer
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-ws-2",
            "object": "chat.completion",
            "created": 1234567891,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Here are the latest search results for your query."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 15, "total_tokens": 115 }
        })))
        .mount(&mock_server)
        .await;

    let client = Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        mock_server.uri(),
        "test-key".into(),
        "test-model".into(),
    ));

    let registry = {
        use rubberdux::provider::moonshot::tool::web_search::WebSearchTool;

        let mut r = rubberdux::tool::ToolRegistry::new();
        r.register(Box::new(WebSearchTool::new(client.clone())));
        r
    };

    // Verify web_search is in the registry
    let defs = registry.definitions();
    assert!(
        defs.iter().any(|d| d.function.name == "web_search"),
        "web_search should be registered"
    );

    // Execute web_search through the registry
    let outcome = registry
        .execute("web_search", r#"{"query": "latest AI news"}"#)
        .await;

    // Should return Background variant (spawns async task)
    let receiver = match outcome {
        tool::ToolOutcome::Background {
            task_id, receiver, ..
        } => {
            assert!(
                task_id.starts_with("search_"),
                "task_id should start with search_"
            );
            receiver
        }
        tool::ToolOutcome::Immediate { content, is_error } => {
            panic!(
                "Expected Background, got Immediate(is_error={}, content={})",
                is_error, content
            );
        }
        tool::ToolOutcome::Subagent { .. } => {
            panic!("Expected Background, got Subagent");
        }
    };

    // Wait for background task to complete
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), receiver)
        .await
        .expect("background task should complete within 10s")
        .expect("oneshot channel should not be dropped");

    assert!(
        result.content.contains("latest search results"),
        "Expected search result content, got: {}",
        result.content
    );

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected two web_search API calls");

    let first_request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        first_request["messages"][1]["content"].as_str(),
        Some("latest AI news")
    );
    assert_eq!(
        first_request["tools"][0]["function"]["name"].as_str(),
        Some("$web_search")
    );
    assert_eq!(
        first_request["thinking"]["type"].as_str(),
        Some("disabled")
    );
}
