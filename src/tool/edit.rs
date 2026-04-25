use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct EditFileTool;

impl super::Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("edit_file.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!("Failed to parse tool arguments: {}", e),
                        is_error: true,
                    };
                }
            };
            execute(&args).await
        })
    }
}

pub async fn execute(args: &serde_json::Value) -> ToolOutcome {
    let file_path = match args["file_path"].as_str() {
        Some(p) => p,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: file_path".into(),
                is_error: true,
            };
        }
    };

    let old_string = match args["old_string"].as_str() {
        Some(s) => s,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: old_string".into(),
                is_error: true,
            };
        }
    };

    let new_string = match args["new_string"].as_str() {
        Some(s) => s,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: new_string".into(),
                is_error: true,
            };
        }
    };

    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            return ToolOutcome::Immediate {
                content: format!("Failed to read file {}: {}", file_path, e),
                is_error: true,
            };
        }
    };

    let count = content.matches(old_string).count();

    if count == 0 {
        return ToolOutcome::Immediate {
            content: format!("old_string not found in {}", file_path),
            is_error: true,
        };
    }

    if count > 1 {
        return ToolOutcome::Immediate {
            content: format!(
                "old_string found {} times in {}. Must be unique. Provide more context.",
                count, file_path
            ),
            is_error: true,
        };
    }

    let new_content = content.replacen(old_string, new_string, 1);

    match std::fs::write(file_path, &new_content) {
        Ok(()) => ToolOutcome::Immediate {
            content: format!("Successfully edited {}", file_path),
            is_error: false,
        },
        Err(e) => ToolOutcome::Immediate {
            content: format!("Failed to write file {}: {}", file_path, e),
            is_error: true,
        },
    }
}
