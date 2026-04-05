use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct WriteFileTool;

impl super::Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("write_file.json")).unwrap()
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::Immediate {
                    content: format!("Failed to parse tool arguments: {}", e),
                    is_error: true,
                },
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
            }
        }
    };

    let content = match args["content"].as_str() {
        Some(c) => c,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: content".into(),
                is_error: true,
            }
        }
    };

    // Create parent directories if needed
    if let Some(parent) = std::path::Path::new(file_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::write(file_path, content) {
        Ok(()) => ToolOutcome::Immediate {
            content: format!("Successfully wrote {} bytes to {}", content.len(), file_path),
            is_error: false,
        },
        Err(e) => ToolOutcome::Immediate {
            content: format!("Failed to write file {}: {}", file_path, e),
            is_error: true,
        },
    }
}
