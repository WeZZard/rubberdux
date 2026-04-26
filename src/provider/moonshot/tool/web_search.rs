use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use super::{FunctionDefinition, ToolDefinition};
use crate::provider::moonshot::api::chat::ChatResponse;
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::{Tool, ToolOutcome};

const WEB_SEARCH_PROMPT: &str = include_str!("WEB_SEARCH.md");

pub struct WebSearchContext {
    pub client: Arc<MoonshotClient>,
}

pub struct WebSearchTool {
    client: Arc<MoonshotClient>,
}

impl WebSearchTool {
    pub fn new(client: Arc<MoonshotClient>) -> Self {
        Self { client }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("web_search.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let ctx = WebSearchContext {
                client: self.client.clone(),
            };
            execute(arguments, Some(ctx)).await
        })
    }
}

pub async fn execute(arguments: &str, context: Option<WebSearchContext>) -> ToolOutcome {
    let ctx = match context {
        Some(c) => c,
        None => {
            return ToolOutcome::Immediate {
                content: arguments.to_owned(),
                is_error: false,
            };
        }
    };

    let Some(query) = query_from_arguments(arguments) else {
        return ToolOutcome::Immediate {
            content: "Web search failed: missing search query".to_owned(),
            is_error: true,
        };
    };

    let task_id = format!("search_{}", generate_task_id());
    let output_dir = PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);
    let output_path = output_dir.join(format!("{}.output", task_id));

    let (tx, rx) = tokio::sync::oneshot::channel();

    let path = output_path.clone();
    let task_id_clone = task_id.clone();
    let tid = task_id.clone();

    tokio::spawn(async move {
        log::info!("Background web search task {} started", task_id_clone);

        let messages = initial_messages(query);
        let web_search_builtin = web_search_builtin_tools();

        // First call: model will call $web_search, API returns tool_calls
        let first_result = ctx
            .client
            .chat(messages.clone(), Some(web_search_builtin.clone()))
            .await;

        let result = match first_result {
            Ok(response) => {
                let choice = match response.choices.first() {
                    Some(c) => c,
                    None => {
                        let _ = tx.send(crate::tool::BackgroundTaskResult {
                            task_id: tid.clone(),
                            content: "(empty search result)".into(),
                        });
                        if let Err(e) = std::fs::write(&path, "(empty search result)") {
                            log::error!("Failed to write search result: {}", e);
                        }
                        log::info!("Background web search task {} completed", task_id_clone);
                        return;
                    }
                };

                if choice.finish_reason == "stop" {
                    // Model responded directly without tool call
                    Ok(response)
                } else if choice.message.tool_calls().is_some() {
                    let follow_up = follow_up_messages(messages, choice.message.clone());
                    ctx.client.chat(follow_up, Some(web_search_builtin)).await
                } else {
                    Ok(response)
                }
            }
            Err(e) => Err(e),
        };

        let result = result; // shadow for the output extraction below

        let output = output_from_result(result);

        if let Err(e) = std::fs::write(&path, &output) {
            log::error!("Failed to write search result: {}", e);
        }

        log::info!("Background web search task {} completed", task_id_clone);

        let _ = tx.send(crate::tool::BackgroundTaskResult {
            task_id: tid,
            content: output,
        });
    });

    ToolOutcome::Background {
        task_id,
        output_path,
        receiver: rx,
    }
}

fn generate_task_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:x}", ts & 0xFFFF_FFFF)
}

fn query_from_arguments(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("query")
                .and_then(|query| query.as_str())
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .map(str::to_owned)
        })
}

fn initial_messages(query: String) -> Vec<Message> {
    vec![
        Message::System {
            content: WEB_SEARCH_PROMPT.to_owned(),
        },
        Message::User {
            content: UserContent::Text(query),
        },
    ]
}

fn web_search_builtin_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        r#type: "builtin_function".to_owned(),
        function: FunctionDefinition {
            name: "$web_search".to_owned(),
            description: None,
            parameters: None,
        },
    }]
}

fn follow_up_messages(mut messages: Vec<Message>, assistant_message: Message) -> Vec<Message> {
    let tool_calls = assistant_message.tool_calls().cloned().unwrap_or_default();
    messages.push(assistant_message);
    for tc in tool_calls {
        messages.push(Message::Tool {
            tool_call_id: tc.id,
            name: Some("$web_search".to_owned()),
            content: tc.function.arguments,
        });
    }
    messages
}

fn output_from_result(result: Result<ChatResponse, crate::error::Error>) -> String {
    match result {
        Ok(response) => response
            .choices
            .first()
            .map(|c| c.message.content_text().to_owned())
            .unwrap_or_else(|| "(empty search result)".into()),
        Err(e) => format!("Web search failed: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::provider::moonshot::api::chat::{ChatChoice, Usage};
    use crate::provider::moonshot::tool::{FunctionCall, ToolCall};

    fn dummy_client() -> Arc<MoonshotClient> {
        Arc::new(MoonshotClient::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_owned(),
            "test-key".to_owned(),
            "test-model".to_owned(),
        ))
    }

    fn chat_response(choices: Vec<ChatChoice>) -> ChatResponse {
        ChatResponse {
            id: "cmpl-test".to_owned(),
            choices,
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_tokens: 0,
            },
        }
    }

    fn assistant_choice(content: &str) -> ChatChoice {
        ChatChoice {
            message: Message::Assistant {
                content: Some(content.to_owned()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
            finish_reason: "stop".to_owned(),
        }
    }

    #[test]
    fn public_tool_definition_exposes_regular_function_tool() {
        let tool = WebSearchTool::new(dummy_client());

        assert_eq!(tool.name(), "web_search");

        let definition = tool.definition();
        assert_eq!(definition.r#type, "function");
        assert_eq!(definition.function.name, "web_search");

        let parameters = definition.function.parameters.as_ref().unwrap();
        assert_eq!(parameters["type"].as_str(), Some("object"));
        assert_eq!(
            parameters["properties"]["query"]["type"].as_str(),
            Some("string")
        );
        assert!(
            parameters["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("query"))
        );
    }

    #[test]
    fn provider_builtin_definition_uses_private_moonshot_name() {
        let tools = web_search_builtin_tools();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].r#type, "builtin_function");
        assert_eq!(tools[0].function.name, "$web_search");
        assert_eq!(tools[0].function.description, None);
        assert_eq!(tools[0].function.parameters, None);
    }

    #[test]
    fn query_argument_is_used() {
        let query = query_from_arguments(r#"{"query":"Rust programming language"}"#);

        assert_eq!(query.as_deref(), Some("Rust programming language"));
    }

    #[test]
    fn query_argument_is_trimmed() {
        let query = query_from_arguments(r#"{"query":"  Rust programming language  "}"#);

        assert_eq!(query.as_deref(), Some("Rust programming language"));
    }

    #[test]
    fn blank_query_returns_none() {
        let query = query_from_arguments(r#"{"query":"   "}"#);

        assert_eq!(query, None);
    }

    #[test]
    fn malformed_arguments_return_none() {
        let query = query_from_arguments("not json");

        assert_eq!(query, None);
    }

    #[test]
    fn missing_query_returns_none() {
        let query = query_from_arguments(r#"{"unexpected":"value"}"#);

        assert_eq!(query, None);
    }

    #[test]
    fn initial_provider_messages_use_selected_query() {
        let messages = initial_messages("expected query".to_owned());

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::System { content } if content == WEB_SEARCH_PROMPT
        ));
        assert!(matches!(
            &messages[1],
            Message::User { content: UserContent::Text(content) } if content == "expected query"
        ));
    }

    #[test]
    fn follow_up_messages_echo_provider_tool_call_arguments() {
        let initial = initial_messages("expected query".to_owned());
        let arguments = r#"{"search_result":{"id":"mock_search"}}"#.to_owned();
        let assistant_message = Message::Assistant {
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: Some(vec![ToolCall {
                index: Some(0),
                id: "tool_ws_001".to_owned(),
                r#type: "builtin_function".to_owned(),
                function: FunctionCall {
                    name: "$web_search".to_owned(),
                    arguments: arguments.clone(),
                },
                depends_on: None,
            }]),
            partial: None,
        };

        let follow_up = follow_up_messages(initial, assistant_message);

        assert_eq!(follow_up.len(), 4);
        assert!(matches!(&follow_up[2], Message::Assistant { .. }));
        assert!(matches!(
            &follow_up[3],
            Message::Tool { tool_call_id, name, content }
                if tool_call_id == "tool_ws_001"
                    && name.as_deref() == Some("$web_search")
                    && content == &arguments
        ));
    }

    #[test]
    fn output_from_result_returns_final_provider_text() {
        let output = output_from_result(Ok(chat_response(vec![assistant_choice(
            "Here are search results.",
        )])));

        assert_eq!(output, "Here are search results.");
    }

    #[test]
    fn output_from_result_handles_empty_choices() {
        let output = output_from_result(Ok(chat_response(Vec::new())));

        assert_eq!(output, "(empty search result)");
    }

    #[test]
    fn output_from_result_maps_provider_errors_to_tool_errors() {
        let output = output_from_result(Err(crate::error::Error::ProviderApi {
            status: 400,
            body: "bad request".to_owned(),
        }));

        assert_eq!(
            output,
            "Web search failed: provider API error (status 400): bad request"
        );
    }

    #[tokio::test]
    async fn missing_query_returns_local_error_without_provider_request() {
        let outcome = execute(
            r#"{"unexpected":"value"}"#,
            Some(WebSearchContext {
                client: dummy_client(),
            }),
        )
        .await;

        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert_eq!(content, "Web search failed: missing search query");
            }
            ToolOutcome::Background { .. } => {
                panic!("missing query must not start background work")
            }
            ToolOutcome::Subagent { .. } => panic!("missing query must not start a subagent"),
        }
    }
}
