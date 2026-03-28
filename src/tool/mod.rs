pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod web_fetch;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::provider::moonshot::tool::{ToolCall, ToolDefinition};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};

const DEFAULT_TOOLS_DIR: &str = "./tools";

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Context for executing builtin tools that need API access.
pub struct ToolContext {
    pub client: Arc<MoonshotClient>,
    pub system_prompt: String,
    pub last_user_query: String,
    pub assistant_message: Message,
    pub tool_call: ToolCall,
}

pub fn tools_dir() -> PathBuf {
    std::env::var("RUBBERDUX_TOOLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TOOLS_DIR))
}

pub fn load_tool_definitions(dir: &Path) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            log::error!("Failed to read tools directory {:?}: {}", dir, e);
            return definitions;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<ToolDefinition>(&content) {
                Ok(def) => {
                    log::info!("Loaded tool definition: {} from {:?}", def.function.name, path);
                    definitions.push(def);
                }
                Err(e) => log::warn!("Failed to parse tool definition {:?}: {}", path, e),
            },
            Err(e) => log::warn!("Failed to read tool definition {:?}: {}", path, e),
        }
    }

    definitions.sort_by(|a, b| a.function.name.cmp(&b.function.name));
    log::info!("Loaded {} tool definitions", definitions.len());
    definitions
}

/// Returns true if the tool is a Kimi builtin function (name starts with $).
/// Builtin tools are executed server-side — we echo the arguments back as the result.
pub fn is_builtin_tool(name: &str) -> bool {
    name.starts_with('$')
}

pub async fn execute_tool(
    name: &str,
    arguments: &str,
    context: Option<ToolContext>,
) -> ToolResult {
    // $web_search: spawn background one-shot model call
    if name == "$web_search" {
        return execute_web_search_background(arguments, context).await;
    }

    // Other Kimi builtins: echo arguments back
    if is_builtin_tool(name) {
        return ToolResult {
            content: arguments.to_owned(),
            is_error: false,
        };
    }

    let args: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => {
            return ToolResult {
                content: format!("Failed to parse tool arguments: {}", e),
                is_error: true,
            }
        }
    };

    match name {
        "bash" => bash::execute(&args).await,
        "read_file" => read::execute(&args).await,
        "write_file" => write::execute(&args).await,
        "edit_file" => edit::execute(&args).await,
        "glob" => glob::execute(&args).await,
        "grep" => grep::execute(&args).await,
        "web_fetch" => web_fetch::execute(&args).await,
        _ => ToolResult {
            content: format!("Unknown tool: {}", name),
            is_error: true,
        },
    }
}

async fn execute_web_search_background(
    arguments: &str,
    context: Option<ToolContext>,
) -> ToolResult {
    let ctx = match context {
        Some(c) => c,
        None => {
            // No context — fall back to echo (synchronous)
            return ToolResult {
                content: arguments.to_owned(),
                is_error: false,
            };
        }
    };

    let task_id = format!("search_{}", generate_task_id());
    let output_dir = PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);

    let output_path = output_dir.join(format!("{}.output", task_id));
    let output_path_str = output_path.to_string_lossy().to_string();

    let arguments = arguments.to_owned();
    let path = output_path.clone();
    let task_id_clone = task_id.clone();

    tokio::spawn(async move {
        log::info!("Background web search task {} started", task_id_clone);

        // Build one-shot messages: [system, user_query, assistant(tool_calls), tool(search_args)]
        let messages = vec![
            Message::System {
                content: ctx.system_prompt,
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
                let text = response
                    .choices
                    .first()
                    .map(|c| c.message.content_text().to_owned())
                    .unwrap_or_else(|| "(empty search result)".into());
                text
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

    ToolResult {
        content: format!(
            "Searching in background with ID: {}. Output is being written to: {}",
            task_id, output_path_str
        ),
        is_error: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tool_definitions() {
        let defs = load_tool_definitions(Path::new("./tools"));
        assert_eq!(defs.len(), 7);
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let result = execute_tool("nonexistent", "{}", None).await;
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn test_web_search_without_context_echoes_args() {
        let result = execute_tool("$web_search", "{\"test\": true}", None).await;
        assert!(!result.is_error);
        // Without context, falls back to echo
        assert!(result.content.contains("test"));
    }
}
