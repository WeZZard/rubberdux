pub mod api;
pub mod tool;

use serde::{Deserialize, Serialize};
use tool::ToolCall;
use unicode_segmentation::UnicodeSegmentation;

#[cfg(feature = "host")]
use crate::channel::interpreter::{Attachment, InterpretedMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        content: Option<String>,
        #[serde(default)]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial: Option<bool>,
    },
    Tool {
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: MediaUrl },
    VideoUrl { video_url: MediaUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaUrl {
    pub url: String,
}

impl Message {
    pub fn content_text(&self) -> &str {
        match self {
            Message::System { content } => content,
            Message::User { content } => match content {
                UserContent::Text(t) => t,
                UserContent::Parts(parts) => {
                    for part in parts {
                        if let ContentPart::Text { text } = part {
                            return text;
                        }
                    }
                    ""
                }
            },
            Message::Assistant { content, .. } => content.as_deref().unwrap_or(""),
            Message::Tool { content, .. } => content,
        }
    }

    pub fn tool_calls(&self) -> Option<&Vec<ToolCall>> {
        match self {
            Message::Assistant { tool_calls, .. } => tool_calls.as_ref(),
            _ => None,
        }
    }

    pub fn reasoning_content(&self) -> Option<&str> {
        match self {
            Message::Assistant {
                reasoning_content, ..
            } => reasoning_content.as_deref(),
            _ => None,
        }
    }

    #[cfg(feature = "host")]
    pub fn from_interpreted(interpreted: &InterpretedMessage) -> Self {
        if interpreted.attachments.is_empty() {
            return Message::User {
                content: UserContent::Text(interpreted.text.clone()),
            };
        }

        let mut parts = Vec::new();

        if !interpreted.text.is_empty() {
            parts.push(ContentPart::Text {
                text: interpreted.text.clone(),
            });
        }

        for attachment in &interpreted.attachments {
            match attachment {
                Attachment::Image { base64, mime_type } => {
                    parts.push(ContentPart::ImageUrl {
                        image_url: MediaUrl {
                            url: format!("data:{};base64,{}", mime_type, base64),
                        },
                    });
                }
                Attachment::Video { base64, mime_type } => {
                    parts.push(ContentPart::VideoUrl {
                        video_url: MediaUrl {
                            url: format!("data:{};base64,{}", mime_type, base64),
                        },
                    });
                }
            }
        }

        Message::User {
            content: UserContent::Parts(parts),
        }
    }
}

pub struct MoonshotClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl MoonshotClient {
    pub fn new(http: reqwest::Client, base_url: String, api_key: String, model: String) -> Self {
        Self {
            http,
            base_url,
            api_key,
            model,
        }
    }

    pub fn from_env() -> Self {
        let base_url = std::env::var("RUBBERDUX_LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.moonshot.ai/v1".into());
        let api_key = std::env::var("RUBBERDUX_LLM_API_KEY").unwrap_or_default();
        let model =
            std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());

        let mut builder = reqwest::ClientBuilder::new();
        if let Ok(user_agent) = std::env::var("RUBBERDUX_LLM_USER_AGENT") {
            builder = builder.user_agent(user_agent);
        }
        let http = builder.build().expect("failed to build HTTP client");

        Self {
            http,
            base_url,
            api_key,
            model,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    pub(crate) fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let sys = Message::System {
            content: "You are helpful.".into(),
        };
        let json = serde_json::to_value(&sys).unwrap();
        assert_eq!(json["role"], "system");
        assert_eq!(json["content"], "You are helpful.");

        let user = Message::User {
            content: UserContent::Text("Hello".into()),
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Hello");

        let asst = Message::Assistant {
            content: Some("Hi!".into()),
            reasoning_content: None,
            tool_calls: None,
            partial: None,
        };
        let json = serde_json::to_value(&asst).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "Hi!");
        assert!(json.get("tool_calls").is_none());
        assert!(json.get("partial").is_none());

        let tool = Message::Tool {
            tool_call_id: "call_123".into(),
            name: None,
            content: "result".into(),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_123");
        assert_eq!(json["content"], "result");
    }

    #[test]
    fn test_user_content_text_serialization() {
        let content = UserContent::Text("Hello".into());
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.is_string());
        assert_eq!(json, "Hello");
    }

    #[test]
    fn test_user_content_parts_serialization() {
        let content = UserContent::Parts(vec![
            ContentPart::Text {
                text: "Explain this".into(),
            },
            ContentPart::ImageUrl {
                image_url: MediaUrl {
                    url: "data:image/jpeg;base64,abc123".into(),
                },
            },
        ]);
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.is_array());
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "Explain this");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/jpeg;base64,abc123");
    }

    #[test]
    fn test_multimodal_user_message_serialization() {
        let msg = Message::User {
            content: UserContent::Parts(vec![
                ContentPart::Text {
                    text: "What is this?".into(),
                },
                ContentPart::ImageUrl {
                    image_url: MediaUrl {
                        url: "data:image/png;base64,xyz".into(),
                    },
                },
            ]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert!(json["content"].is_array());
    }

    #[test]
    fn test_assistant_reasoning_content_always_serialized() {
        // When reasoning_content is Some(""), it must serialize as "reasoning_content": ""
        // not be omitted — Kimi requires it for thinking mode
        let msg = Message::Assistant {
            content: Some("".into()),
            reasoning_content: Some("".into()),
            tool_calls: None,
            partial: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(
            json.get("reasoning_content").is_some(),
            "reasoning_content must be present even when empty: {}",
            serde_json::to_string_pretty(&json).unwrap()
        );
        assert_eq!(json["reasoning_content"], "");
    }

    #[test]
    fn test_assistant_reasoning_content_none_serializes() {
        // When reasoning_content is None, check what happens
        let msg = Message::Assistant {
            content: Some("Hello".into()),
            reasoning_content: None,
            tool_calls: None,
            partial: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        // With #[serde(default)], None serializes as null
        // Kimi needs it present (even as null or ""), not omitted
        let has_field = json.get("reasoning_content").is_some();
        println!(
            "reasoning_content when None: {:?}",
            json.get("reasoning_content")
        );
        // This test documents current behavior
        assert!(
            has_field,
            "reasoning_content should be present (not omitted) since we removed skip_serializing_if"
        );
    }

    #[test]
    fn test_builtin_tool_call_roundtrip() {
        // Simulate the $web_search flow:
        // 1. API returns assistant with tool_calls and no reasoning_content
        // 2. We deserialize, ensure reasoning_content is Some("")
        // 3. We serialize back — reasoning_content must be present
        let api_response = r#"{
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "tool_abc",
                "type": "builtin_function",
                "function": {
                    "name": "$web_search",
                    "arguments": "{\"search_result\":{\"search_id\":\"123\"}}"
                }
            }]
        }"#;

        let mut msg: Message = serde_json::from_str(api_response).unwrap();

        // Simulate the fix: ensure reasoning_content is present and non-empty
        if let Message::Assistant {
            reasoning_content, ..
        } = &mut msg
        {
            if reasoning_content.is_none()
                || reasoning_content.as_ref().is_some_and(|s| s.is_empty())
            {
                *reasoning_content = Some("(tool call)".to_owned());
            }
        }

        // Serialize back
        let json = serde_json::to_value(&msg).unwrap();

        // reasoning_content MUST be present, a string, and NON-EMPTY
        // Kimi treats empty string as "missing" and rejects the request
        assert!(
            json.get("reasoning_content").is_some(),
            "reasoning_content must be present"
        );
        assert!(
            json["reasoning_content"].is_string(),
            "reasoning_content must be a string, not null"
        );
        assert!(
            !json["reasoning_content"].as_str().unwrap().is_empty(),
            "reasoning_content must not be empty — Kimi rejects empty string"
        );

        // tool_calls must also be present
        assert!(json.get("tool_calls").is_some());
    }

    /// Shared harness: builds history with a given tool result message,
    /// calls real Moonshot API, and returns whether the model tried to poll.
    async fn run_background_tool_result_trial(label: &str, tool_result_content: &str) -> bool {
        let client = MoonshotClient::from_env();
        let mut registry = crate::tool::ToolRegistry::new();
        registry.register(Box::new(crate::tool::bash::BashTool));
        registry.register(Box::new(crate::tool::web_fetch::WebFetchTool));
        registry.register(Box::new(crate::tool::read::ReadFileTool));
        registry.register(Box::new(crate::tool::write::WriteFileTool));
        registry.register(Box::new(crate::tool::edit::EditFileTool));
        registry.register(Box::new(crate::tool::glob::GlobTool));
        registry.register(Box::new(crate::tool::grep::GrepTool));
        let tools = registry.definitions();
        let system_prompt = "You are a helpful assistant.";

        let mut history: Vec<Message> = vec![
            Message::User {
                content: UserContent::Text("Fetch https://example.com for me".into()),
            },
            Message::Assistant {
                content: Some("Let me fetch that page for you.".into()),
                reasoning_content: Some("The user wants me to fetch a URL.".into()),
                tool_calls: Some(vec![tool::ToolCall {
                    index: Some(0),
                    id: "tool_fetch_001".into(),
                    r#type: "function".into(),
                    function: tool::FunctionCall {
                        name: "web_fetch".into(),
                        arguments: "{\"url\": \"https://example.com\"}".into(),
                    },
                    depends_on: None,
                }]),
                partial: None,
            },
            Message::Tool {
                tool_call_id: "tool_fetch_001".into(),
                name: None,
                content: tool_result_content.into(),
            },
        ];

        eprintln!("\n{}", "=".repeat(80));
        eprintln!("=== TRIAL: {} ===", label);
        eprintln!("Tool result: {:?}", tool_result_content);
        eprintln!("{}", "=".repeat(80));

        let mut polled = false;
        let max_iterations = 3;
        for iteration in 1..=max_iterations {
            let mut messages = vec![Message::System {
                content: system_prompt.to_owned(),
            }];
            messages.extend_from_slice(&history);

            let response = client.chat(messages, Some(tools.clone())).await.unwrap();
            let choice = &response.choices[0];

            eprintln!(
                "\n  [Call #{}] finish_reason={}",
                iteration, choice.finish_reason
            );
            if let Some(rc) = choice.message.reasoning_content() {
                eprintln!("  reasoning: {}", rc);
            }

            history.push(choice.message.clone());

            if choice.finish_reason == "stop" {
                let text = choice.message.content_text();
                let stopped_preview: String = text.graphemes(true).take(120).collect();
                eprintln!("  STOPPED: {}", stopped_preview);
                break;
            }

            if let Some(tool_calls) = choice.message.tool_calls() {
                for call in tool_calls {
                    eprintln!(
                        "  TOOL CALL: {}({})",
                        call.function.name, call.function.arguments
                    );
                    polled = true;

                    // Don't actually execute — return a generic error to prevent infinite loops
                    history.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        name: None,
                        content: "Error: this operation is not available.".into(),
                    });
                }
            }
        }

        eprintln!("  RESULT: polled={}\n", polled);
        polled
    }

    /// Tests multiple tool result message variants against the real Moonshot API
    /// to find which ones prevent the model from polling background task output.
    ///
    /// Run with: cargo test test_debug_background_tool_model_call -- --nocapture --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore] // requires real API credentials
    async fn test_debug_background_tool_model_call() {
        dotenvy::dotenv().ok();

        // Use the default format_tool_outcome to generate the tool result message
        let (_tx, _rx) = tokio::sync::oneshot::channel();
        let default_bg_message =
            crate::tool::format_tool_outcome(&crate::tool::ToolOutcome::Background {
                task_id: "fetch_abc123".into(),
                output_path: std::path::PathBuf::from("./sessions/tasks/fetch_abc123.output"),
                receiver: _rx,
            });

        let trials = vec![(
            "Default format_tool_outcome (no file path)",
            default_bg_message.as_str(),
        )];

        eprintln!("\n{}\n", "=".repeat(80));
        eprintln!("BACKGROUND TOOL RESULT — MODEL BEHAVIOR TRIALS");
        eprintln!(
            "Testing {} variants against real Moonshot API",
            trials.len()
        );
        eprintln!("\n{}", "=".repeat(80));

        let mut results = Vec::new();
        for (label, content) in &trials {
            let polled = run_background_tool_result_trial(label, content).await;
            results.push((label, polled));
        }

        eprintln!("\n{}", "=".repeat(80));
        eprintln!("SUMMARY");
        eprintln!("{}", "=".repeat(80));
        for (label, polled) in &results {
            let status = if *polled {
                "POLLED (bad)"
            } else {
                "CLEAN (good)"
            };
            eprintln!("  {} => {}", label, status);
        }
        eprintln!("{}\n", "=".repeat(80));
    }

    /// Experiment: observe whether recursive tool calls form a tree or linked list.
    ///
    /// Scenario: user asks for info that requires multiple sources. After each
    /// tool result, we observe if the model calls ONE tool (linked list) or
    /// MULTIPLE tools (tree) in the next turn.
    ///
    /// Run with: cargo test test_tool_call_recursion_shape -- --nocapture --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore] // requires real API credentials
    async fn test_tool_call_recursion_shape() {
        dotenvy::dotenv().ok();
        let client = MoonshotClient::from_env();
        let mut registry = crate::tool::ToolRegistry::new();
        registry.register(Box::new(crate::tool::bash::BashTool));
        registry.register(Box::new(crate::tool::web_fetch::WebFetchTool));
        registry.register(Box::new(crate::tool::read::ReadFileTool));
        registry.register(Box::new(crate::tool::write::WriteFileTool));
        registry.register(Box::new(crate::tool::edit::EditFileTool));
        registry.register(Box::new(crate::tool::glob::GlobTool));
        registry.register(Box::new(crate::tool::grep::GrepTool));
        let tools = registry.definitions();

        let system_prompt = "You are a helpful assistant.";

        let mut history: Vec<Message> = vec![Message::User {
            content: UserContent::Text(
                "Compare the latest news about Google and Apple. Search for both.".into(),
            ),
        }];

        eprintln!("\n{}", "=".repeat(80));
        eprintln!("TOOL CALL RECURSION SHAPE EXPERIMENT");
        eprintln!("{}", "=".repeat(80));

        let max_turns = 6;
        for turn in 1..=max_turns {
            let mut messages = vec![Message::System {
                content: system_prompt.to_owned(),
            }];
            messages.extend_from_slice(&history);

            let response = client.chat(messages, Some(tools.clone())).await.unwrap();
            let choice = &response.choices[0];

            let tool_calls = choice.message.tool_calls();
            let num_tools = tool_calls.map(|tc| tc.len()).unwrap_or(0);

            eprintln!(
                "\n  [Turn {}] finish_reason={} tool_calls={}",
                turn, choice.finish_reason, num_tools
            );

            if let Some(rc) = choice.message.reasoning_content() {
                let rc_short: String = rc.graphemes(true).take(200).collect();
                eprintln!("  reasoning: {}...", rc_short);
            }

            if num_tools > 0 {
                let tc = tool_calls.unwrap();
                for (i, call) in tc.iter().enumerate() {
                    eprintln!(
                        "    tool[{}]: {}({})",
                        i,
                        call.function.name,
                        &call.function.arguments[..call.function.arguments.len().min(80)]
                    );
                }

                // KEY OBSERVATION: does the model call 1 tool or multiple?
                if num_tools > 1 {
                    eprintln!("  >>> TREE: model called {} tools in one turn", num_tools);
                } else {
                    eprintln!("  >>> CHAIN: model called 1 tool");
                }
            } else {
                let text = choice.message.content_text();
                let short: String = text.graphemes(true).take(150).collect();
                eprintln!("  response: {}...", short);
            }

            history.push(choice.message.clone());

            if choice.finish_reason == "stop" {
                eprintln!("\n  Model stopped at turn {}", turn);
                break;
            }

            // Provide fake tool results for each tool call
            if let Some(tc) = tool_calls {
                for call in tc {
                    let fake_result = format!(
                        "Search results for {}: Some relevant news articles found.",
                        call.function.name
                    );
                    history.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        name: if call.function.name.starts_with('$') {
                            Some(call.function.name.clone())
                        } else {
                            None
                        },
                        content: fake_result,
                    });
                    eprintln!("  <- provided fake result for {}", call.function.name);
                }
            }
        }

        eprintln!("\n{}", "=".repeat(80));
    }

    /// Experiment: verify the Moonshot API accepts multiple Tool messages
    /// as responses to multiple tool calls in one assistant turn, and the
    /// model processes all results together in the next turn.
    ///
    /// Run with: cargo test test_batch_tool_results -- --nocapture --ignored
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn test_batch_tool_results() {
        dotenvy::dotenv().ok();
        let client = MoonshotClient::from_env();
        let mut registry = crate::tool::ToolRegistry::new();
        registry.register(Box::new(crate::tool::bash::BashTool));
        registry.register(Box::new(crate::tool::web_fetch::WebFetchTool));
        registry.register(Box::new(crate::tool::read::ReadFileTool));
        registry.register(Box::new(crate::tool::write::WriteFileTool));
        registry.register(Box::new(crate::tool::edit::EditFileTool));
        registry.register(Box::new(crate::tool::glob::GlobTool));
        registry.register(Box::new(crate::tool::grep::GrepTool));
        let tools = registry.definitions();

        // Step 1: Get the model to call multiple tools
        let mut history: Vec<Message> = vec![Message::User {
            content: UserContent::Text(
                "Fetch both https://example.com and https://example.org for me.".into(),
            ),
        }];

        let mut messages = vec![Message::System {
            content: "You are a helpful assistant.".to_owned(),
        }];
        messages.extend_from_slice(&history);

        let response = client.chat(messages, Some(tools.clone())).await.unwrap();
        let choice = &response.choices[0];
        let tc = choice.message.tool_calls();
        let num = tc.map(|t| t.len()).unwrap_or(0);

        eprintln!("\n{}", "=".repeat(80));
        eprintln!("BATCH TOOL RESULTS EXPERIMENT");
        eprintln!("{}", "=".repeat(80));
        eprintln!("\nStep 1: Model called {} tools", num);

        if num < 2 {
            eprintln!("Model didn't call multiple tools. Trying to force it...");
            // If model called only 1, skip the experiment
            eprintln!("SKIPPED — model needs to call 2+ tools for this test");
            return;
        }

        history.push(choice.message.clone());

        // Step 2: Provide ALL tool results at once
        let tool_calls = tc.unwrap();
        for (i, call) in tool_calls.iter().enumerate() {
            eprintln!(
                "  tool[{}]: {}({})",
                i,
                call.function.name,
                &call.function.arguments[..call.function.arguments.len().min(60)]
            );
            let result_content = format!(
                "Content from tool call {}: This is fake content for {}.",
                i, call.function.name
            );
            history.push(Message::Tool {
                tool_call_id: call.id.clone(),
                name: if call.function.name.starts_with('$') {
                    Some(call.function.name.clone())
                } else {
                    None
                },
                content: result_content,
            });
        }

        eprintln!(
            "\nStep 2: Provided {} tool results in one batch",
            tool_calls.len()
        );

        // Step 3: Call the model again — does it see ALL results?
        let mut messages = vec![Message::System {
            content: "You are a helpful assistant.".to_owned(),
        }];
        messages.extend_from_slice(&history);

        let response = client.chat(messages, Some(tools.clone())).await.unwrap();
        let choice = &response.choices[0];

        eprintln!(
            "\nStep 3: Model response (finish_reason={})",
            choice.finish_reason
        );
        if let Some(rc) = choice.message.reasoning_content() {
            let rc_preview: String = rc.graphemes(true).take(300).collect();
            eprintln!("  reasoning: {}", rc_preview);
        }
        let text = choice.message.content_text();
        let text_preview: String = text.graphemes(true).take(300).collect();
        eprintln!("  content: {}", text_preview);

        let new_tc = choice.message.tool_calls().map(|t| t.len()).unwrap_or(0);
        eprintln!("  new tool_calls: {}", new_tc);

        eprintln!(
            "\nCONCLUSION: API {} batch tool results",
            if choice.finish_reason == "stop" || new_tc > 0 {
                "ACCEPTS"
            } else {
                "REJECTS"
            }
        );
        eprintln!("{}\n", "=".repeat(80));
    }
}
