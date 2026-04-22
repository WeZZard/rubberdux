use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

use super::ToolOutcome;

pub struct WebFetchTool;

impl super::Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("web_fetch.json"))
            .expect("web_fetch.json must be valid ToolDefinition")
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

const MAX_CONTENT_LENGTH: usize = 100_000;
const FETCH_TIMEOUT_SECS: u64 = 30;

pub async fn execute(args: &serde_json::Value) -> ToolOutcome {
    let url = match args["url"].as_str() {
        Some(u) => u,
        None => {
            return ToolOutcome::Immediate {
                content: "Missing required parameter: url".into(),
                is_error: true,
            };
        }
    };

    let task_id = format!("fetch_{}", generate_task_id());
    let output_dir = PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);

    let output_path = output_dir.join(format!("{}.output", task_id));

    let (tx, rx) = tokio::sync::oneshot::channel();

    let url = url.to_owned();
    let path = output_path.clone();
    let tid = task_id.clone();

    tokio::spawn(async move {
        let content = match fetch_url(&url).await {
            Ok(text) => text,
            Err(e) => format!("Failed to fetch {}: {}", url, e),
        };

        if let Err(e) = std::fs::write(&path, &content) {
            log::error!("Failed to write fetch result: {}", e);
        }

        log::info!("Background fetch task {} completed", path.display());

        let _ = tx.send(super::BackgroundTaskResult {
            task_id: tid,
            content,
        });
    });

    ToolOutcome::Background { task_id, output_path, receiver: rx }
}

async fn fetch_url(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch {}: {}", url, e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} for {}", status, url));
    }

    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response body from {}: {}", url, e))?;

    let markdown = htmd::convert(&html).unwrap_or_else(|_| html);

    if markdown.len() > MAX_CONTENT_LENGTH {
        let truncated: String = markdown.chars().take(MAX_CONTENT_LENGTH).collect();
        Ok(format!(
            "{}\n\n[Content truncated at {} characters]",
            truncated,
            MAX_CONTENT_LENGTH
        ))
    } else {
        Ok(markdown)
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
