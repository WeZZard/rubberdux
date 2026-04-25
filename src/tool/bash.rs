use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

const MAX_OUTPUT_LENGTH: usize = 100_000;

fn truncate_output(content: String) -> String {
    if content.len() <= MAX_OUTPUT_LENGTH {
        return content;
    }
    let end = content.floor_char_boundary(MAX_OUTPUT_LENGTH);
    format!(
        "{}\n\n[Output truncated at {} characters]",
        &content[..end],
        end
    )
}

pub struct BashTool;

impl super::Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("bash.json"))
            .expect("bash.json must be valid ToolDefinition")
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
    let command = match args["command"].as_str() {
        Some(c) => c,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: command".into(),
                is_error: true,
            };
        }
    };

    let run_in_bg = args["run_in_background"].as_bool().unwrap_or(false);
    let timeout_ms = args["timeout"].as_u64().unwrap_or(120_000);

    if run_in_bg {
        execute_background(command).await
    } else {
        execute_sync(command, timeout_ms).await
    }
}

async fn execute_sync(command: &str, timeout_ms: u64) -> ToolOutcome {
    let timeout = std::time::Duration::from_millis(timeout_ms);

    let result = tokio::time::timeout(
        timeout,
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut content = String::new();
            if !stdout.is_empty() {
                content.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str("STDERR: ");
                content.push_str(&stderr);
            }
            if content.is_empty() {
                content = "(no output)".into();
            }

            ToolOutcome::Immediate {
                content: truncate_output(content),
                is_error: !output.status.success(),
            }
        }
        Ok(Err(e)) => ToolOutcome::Immediate {
            content: format!("Failed to execute command: {}", e),
            is_error: true,
        },
        Err(_) => ToolOutcome::Immediate {
            content: format!("Command timed out after {}ms", timeout_ms),
            is_error: true,
        },
    }
}

async fn execute_background(command: &str) -> ToolOutcome {
    let task_id = format!("bg_{}", generate_task_id());
    let output_dir = std::path::PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);

    let output_path = output_dir.join(format!("{}.output", task_id));

    let (tx, rx) = tokio::sync::oneshot::channel();

    let command = command.to_owned();
    let path = output_path.clone();
    let tid = task_id.clone();

    tokio::spawn(async move {
        let result = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await;

        let content = match result {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    text.push_str("\nSTDERR: ");
                    text.push_str(&stderr);
                }
                if !output.status.success() {
                    text.push_str(&format!("\nExit code: {}", output.status));
                }
                text
            }
            Err(e) => format!("Failed to execute command: {}", e),
        };

        let content = truncate_output(content);

        if let Err(e) = std::fs::write(&path, &content) {
            log::error!("Failed to write background task output: {}", e);
        }

        log::info!("Background task {} completed", path.display());

        let _ = tx.send(super::BackgroundTaskResult {
            task_id: tid,
            content,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_output_under_limit() {
        let short = "hello world".to_owned();
        assert_eq!(truncate_output(short.clone()), short);
    }

    #[test]
    fn test_truncate_output_over_limit() {
        let long = "x".repeat(MAX_OUTPUT_LENGTH + 1000);
        let result = truncate_output(long);
        assert!(result.len() < MAX_OUTPUT_LENGTH + 100);
        assert!(result.ends_with("[Output truncated at 100000 characters]"));
    }

    #[test]
    fn test_truncate_output_exact_limit() {
        let exact = "a".repeat(MAX_OUTPUT_LENGTH);
        assert_eq!(truncate_output(exact.clone()), exact);
    }
}
