use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct ReadFileTool;

impl super::Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("read_file.json")).unwrap()
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

    let offset = args["offset"].as_u64().unwrap_or(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

    match std::fs::read_to_string(file_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = if offset > 0 { offset - 1 } else { 0 };
            let end = (start + limit).min(lines.len());

            let numbered: Vec<String> = lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{}\t{}", start + i + 1, line))
                .collect();

            ToolOutcome::Immediate {
                content: numbered.join("\n"),
                is_error: false,
            }
        }
        Err(e) => ToolOutcome::Immediate {
            content: format!("Failed to read file {}: {}", file_path, e),
            is_error: true,
        },
    }
}
