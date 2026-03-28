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

    let url = url.to_owned();

    // Run headless Chrome on a blocking thread (it uses sync API internally)
    match tokio::task::spawn_blocking(move || fetch_rendered(&url)).await {
        Ok(result) => result,
        Err(e) => ToolResult {
            content: format!("Task panicked: {}", e),
            is_error: true,
        },
    }
}

fn fetch_rendered(url: &str) -> ToolResult {
    let browser = match headless_chrome::Browser::default() {
        Ok(b) => b,
        Err(e) => {
            return ToolResult {
                content: format!("Failed to launch browser: {}", e),
                is_error: true,
            }
        }
    };

    let tab = match browser.new_tab() {
        Ok(t) => t,
        Err(e) => {
            return ToolResult {
                content: format!("Failed to create tab: {}", e),
                is_error: true,
            }
        }
    };

    tab.set_default_timeout(std::time::Duration::from_secs(PAGE_LOAD_TIMEOUT_SECS));

    if let Err(e) = tab.navigate_to(url) {
        return ToolResult {
            content: format!("Failed to navigate to {}: {}", url, e),
            is_error: true,
        };
    }

    if let Err(e) = tab.wait_until_navigated() {
        return ToolResult {
            content: format!("Navigation timeout for {}: {}", url, e),
            is_error: true,
        };
    }

    // Get the rendered HTML content
    let html = match tab.get_content() {
        Ok(h) => h,
        Err(e) => {
            return ToolResult {
                content: format!("Failed to get page content: {}", e),
                is_error: true,
            }
        }
    };

    // Convert HTML to markdown
    let markdown = htmd::convert(&html).unwrap_or_else(|_| html);

    // Truncate if too long
    let content = if markdown.len() > MAX_CONTENT_LENGTH {
        format!(
            "{}\n\n[Content truncated at {} characters]",
            &markdown[..MAX_CONTENT_LENGTH],
            MAX_CONTENT_LENGTH
        )
    } else {
        markdown
    };

    ToolResult {
        content,
        is_error: false,
    }
}
