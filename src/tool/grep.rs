use std::future::Future;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct GrepTool;

impl super::Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("grep.json")).unwrap()
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
    let pattern = match args["pattern"].as_str() {
        Some(p) => p,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: pattern".into(),
                is_error: true,
            }
        }
    };

    let path = args["path"].as_str().unwrap_or(".");
    let glob_filter = args["glob"].as_str();
    let line_numbers = args["include_line_numbers"].as_bool().unwrap_or(true);

    // Try ripgrep first, fall back to grep
    let mut cmd = if which_exists("rg").await {
        let mut c = tokio::process::Command::new("rg");
        if line_numbers {
            c.arg("-n");
        }
        if let Some(g) = glob_filter {
            c.arg("--glob").arg(g);
        }
        c.arg("--max-count").arg("250");
        c.arg(pattern).arg(path);
        c
    } else {
        let mut c = tokio::process::Command::new("grep");
        c.arg("-r");
        if line_numbers {
            c.arg("-n");
        }
        if let Some(g) = glob_filter {
            c.arg("--include").arg(g);
        }
        c.arg(pattern).arg(path);
        c
    };

    match cmd.output().await {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.trim().is_empty() {
                ToolOutcome::Immediate {
                    content: format!("No matches found for pattern: {}", pattern),
                    is_error: false,
                }
            } else {
                ToolOutcome::Immediate {
                    content: stdout.trim().to_owned(),
                    is_error: false,
                }
            }
        }
        Err(e) => ToolOutcome::Immediate {
            content: format!("Failed to execute grep: {}", e),
            is_error: true,
        },
    }
}

async fn which_exists(cmd: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(cmd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
