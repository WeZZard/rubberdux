use std::path::PathBuf;
use std::sync::Arc;

use super::{FunctionDefinition, ToolCall, ToolDefinition};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::ToolOutcome;

const WEB_SEARCH_PROMPT: &str = include_str!("WEB_SEARCH.md");

pub struct WebSearchContext {
    pub client: Arc<MoonshotClient>,
    pub last_user_query: String,
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

    let task_id = format!("search_{}", generate_task_id());
    let output_dir = PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);
    let output_path = output_dir.join(format!("{}.output", task_id));

    let (tx, rx) = tokio::sync::oneshot::channel();

    let arguments = arguments.to_owned();
    let path = output_path.clone();
    let task_id_clone = task_id.clone();
    let tid = task_id.clone();

    tokio::spawn(async move {
        log::info!("Background web search task {} started", task_id_clone);

        // One-shot $web_search: send user query with $web_search builtin enabled.
        // The Moonshot platform triggers the search internally.
        // thinking: disabled is set automatically by chat() when $web_search is present.
        let messages = vec![
            Message::System {
                content: WEB_SEARCH_PROMPT.to_owned(),
            },
            Message::User {
                content: UserContent::Text(ctx.last_user_query),
            },
        ];

        let web_search_builtin = vec![ToolDefinition {
            r#type: "builtin_function".to_owned(),
            function: FunctionDefinition {
                name: "$web_search".to_owned(),
                description: None,
                parameters: None,
            },
        }];

        // First call: model will call $web_search, API returns tool_calls
        let first_result = ctx.client.chat(messages.clone(), Some(web_search_builtin.clone())).await;

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
                } else if let Some(tool_calls) = choice.message.tool_calls() {
                    // Model called $web_search — echo arguments back as per Moonshot docs
                    let mut follow_up = messages;
                    follow_up.push(choice.message.clone());
                    for tc in tool_calls {
                        follow_up.push(Message::Tool {
                            tool_call_id: tc.id.clone(),
                            name: Some("$web_search".to_owned()),
                            content: tc.function.arguments.clone(),
                        });
                    }
                    ctx.client.chat(follow_up, Some(web_search_builtin)).await
                } else {
                    Ok(response)
                }
            }
            Err(e) => Err(e),
        };

        let result = result; // shadow for the output extraction below

        let output = match result {
            Ok(response) => response
                .choices
                .first()
                .map(|c| c.message.content_text().to_owned())
                .unwrap_or_else(|| "(empty search result)".into()),
            Err(e) => {
                format!("Web search failed: {}", e)
            }
        };

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
