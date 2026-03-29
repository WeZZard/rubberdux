use std::path::PathBuf;
use std::sync::Arc;

use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::ToolOutcome;
use super::ToolCall;

pub struct WebSearchContext {
    pub client: Arc<MoonshotClient>,
    pub web_search_prompt: String,
    pub last_user_query: String,
    pub assistant_message: Message,
    pub tool_call: ToolCall,
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

    let arguments = arguments.to_owned();
    let path = output_path.clone();
    let task_id_clone = task_id.clone();

    tokio::spawn(async move {
        log::info!("Background web search task {} started", task_id_clone);

        let messages = vec![
            Message::System {
                content: ctx.web_search_prompt,
            },
            Message::User {
                content: UserContent::Text(ctx.last_user_query),
            },
            ctx.assistant_message,
            Message::Tool {
                tool_call_id: ctx.tool_call.id,
                name: Some("$web_search".to_owned()),
                content: arguments,
            },
        ];

        let result = ctx.client.chat(messages, None).await;

        let output = match result {
            Ok(response) => {
                response
                    .choices
                    .first()
                    .map(|c| c.message.content_text().to_owned())
                    .unwrap_or_else(|| "(empty search result)".into())
            }
            Err(e) => {
                format!("Web search failed: {}", e)
            }
        };

        if let Err(e) = std::fs::write(&path, &output) {
            log::error!("Failed to write search result: {}", e);
        }

        log::info!("Background web search task {} completed", task_id_clone);
    });

    ToolOutcome::Background { task_id, output_path }
}

fn generate_task_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:x}", ts & 0xFFFF_FFFF)
}
