use std::path::PathBuf;

use super::ToolResult;

const MAX_CONTENT_LENGTH: usize = 100_000;
const PAGE_LOAD_TIMEOUT_SECS: u64 = 30;

pub async fn execute(args: &serde_json::Value) -> ToolResult {
    let url = match args["url"].as_str() {
        Some(u) => u,
        None => {
            return ToolResult {
                content: "Missing required parameter: url".into(),
                is_error: true,
            }
        }
    };

    let task_id = format!("fetch_{}", generate_task_id());
    let output_dir = PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);

    let output_path = output_dir.join(format!("{}.output", task_id));
    let output_path_str = output_path.to_string_lossy().to_string();

    let url = url.to_owned();
    let path = output_path.clone();

    tokio::task::spawn_blocking(move || {
        let content = match fetch_rendered(&url) {
            Ok(text) => text,
            Err(e) => format!("Failed to fetch {}: {}", url, e),
        };

        if let Err(e) = std::fs::write(&path, &content) {
            log::error!("Failed to write fetch result: {}", e);
        }

        log::info!("Background fetch task {} completed", path.display());
    });

    ToolResult {
        content: format!(
            "Fetching URL in background with ID: {}. Output is being written to: {}",
            task_id, output_path_str
        ),
        is_error: false,
    }
}

fn fetch_rendered(url: &str) -> Result<String, String> {
    let browser = headless_chrome::Browser::default()
        .map_err(|e| format!("Failed to launch browser: {}", e))?;

    let tab = browser
        .new_tab()
        .map_err(|e| format!("Failed to create tab: {}", e))?;

    tab.set_default_timeout(std::time::Duration::from_secs(PAGE_LOAD_TIMEOUT_SECS));

    tab.navigate_to(url)
        .map_err(|e| format!("Failed to navigate to {}: {}", url, e))?;

    tab.wait_until_navigated()
        .map_err(|e| format!("Navigation timeout for {}: {}", url, e))?;

    let html = tab
        .get_content()
        .map_err(|e| format!("Failed to get page content: {}", e))?;

    let markdown = htmd::convert(&html).unwrap_or_else(|_| html);

    if markdown.len() > MAX_CONTENT_LENGTH {
        Ok(format!(
            "{}\n\n[Content truncated at {} characters]",
            &markdown[..MAX_CONTENT_LENGTH],
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
