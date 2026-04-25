use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct GlobTool;

impl super::Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("glob.json")).unwrap()
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
    let pattern = match args["pattern"].as_str() {
        Some(p) => p,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: pattern".into(),
                is_error: true,
            };
        }
    };

    let base_path = args["path"].as_str().unwrap_or(".");

    let full_pattern = if pattern.starts_with('/') {
        pattern.to_owned()
    } else {
        format!("{}/{}", base_path, pattern)
    };

    // Use find command for glob matching (portable)
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "find {} -path '{}' 2>/dev/null | head -250",
            base_path, full_pattern
        ))
        .output()
        .await;

    // Fallback: use ls with glob
    let output = match output {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => o,
        _ => {
            // Try with bash glob expansion
            match tokio::process::Command::new("bash")
                .arg("-c")
                .arg(format!("ls -1 {} 2>/dev/null | head -250", full_pattern))
                .output()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!("Failed to execute glob: {}", e),
                        is_error: true,
                    };
                }
            }
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        ToolOutcome::Immediate {
            content: format!("No files matching pattern: {}", pattern),
            is_error: false,
        }
    } else {
        ToolOutcome::Immediate {
            content: stdout.trim().to_owned(),
            is_error: false,
        }
    }
}
