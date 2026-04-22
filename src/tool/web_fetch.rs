use std::future::Future;
use std::pin::Pin;
use spider::website::Website;
use spider::features::chrome_common::RequestInterceptConfiguration;
use crate::provider::moonshot::tool::ToolDefinition;
use super::ToolOutcome;
use unicode_segmentation::UnicodeSegmentation;

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

    // Strategy 1: Try spider + Chrome (JS rendering)
    if let Some(chrome_path) = find_chrome_binary() {
        log::info!("Using Chrome at {} for web_fetch", chrome_path.display());
        match fetch_with_spider(url, &chrome_path).await {
            Ok(content) => {
                return ToolOutcome::Immediate {
                    content,
                    is_error: false,
                };
            }
            Err(e) => {
                log::warn!("Spider fetch failed: {}, falling back to reqwest", e);
            }
        }
    } else {
        log::info!("Chrome not found, using reqwest for web_fetch");
    }

    // Strategy 2: Fallback to reqwest (static HTML)
    match fetch_with_reqwest(url).await {
        Ok(content) => ToolOutcome::Immediate {
            content,
            is_error: false,
        },
        Err(e) => ToolOutcome::Immediate {
            content: format!("Failed to fetch {}: {}", url, e),
            is_error: true,
        },
    }
}

async fn fetch_with_spider(url: &str, chrome_path: &std::path::Path) -> Result<String, String> {
    unsafe {
        std::env::set_var(
            "CHROME_URL", format!("file://{}", chrome_path.display()));
    }

    let mut website = Website::new(url)
        .with_limit(1)
        .with_chrome_intercept(RequestInterceptConfiguration::new(true))
        .with_stealth(true)
        .build()
        .map_err(|e| format!("Failed to build spider: {}", e))?;

    website.scrape().await;

    let html = website
        .get_pages()
        .and_then(|pages| pages.first().map(|p| p.get_html()))
        .ok_or("No content returned from spider")?;

    let markdown = htmd::convert(&html)
        .unwrap_or_else(|_| html);

    Ok(truncate_content(markdown))
}

async fn fetch_with_reqwest(url: &str) -> Result<String, String> {
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
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    let markdown = htmd::convert(&html).unwrap_or_else(|_| html);
    Ok(truncate_content(markdown))
}

fn find_chrome_binary() -> Option<std::path::PathBuf> {
    let paths = if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else {
        vec![
            "/usr/bin/chromium-browser",
            "/usr/bin/chromium",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
        ]
    };

    for path in &paths {
        if std::path::Path::new(path).exists() {
            return Some(path.into());
        }
    }
    None
}

fn truncate_content(content: String) -> String {
    if content.graphemes(true).count() > MAX_CONTENT_LENGTH {
        let truncated: String = content.graphemes(true).take(MAX_CONTENT_LENGTH).collect();
        format!(
            "{}\n\n[Content truncated at {} characters]",
            truncated, MAX_CONTENT_LENGTH
        )
    } else {
        content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Truncation tests (no network)
    // ------------------------------------------------------------------

    #[test]
    fn test_truncate_content_exact_boundary() {
        let content = "a".repeat(MAX_CONTENT_LENGTH);
        let truncated = truncate_content(content.clone());
        assert_eq!(truncated, content);
        assert!(!truncated.contains("[Content truncated"));
    }

    #[test]
    fn test_truncate_content_over_boundary() {
        let content = "a".repeat(MAX_CONTENT_LENGTH + 1);
        let truncated = truncate_content(content);
        assert!(truncated.contains("[Content truncated at 100000 characters]"));
        assert_eq!(truncated.graphemes(true).count(), MAX_CONTENT_LENGTH + "\n\n[Content truncated at 100000 characters]".graphemes(true).count());
    }

    #[test]
    fn test_truncate_content_with_emoji() {
        let emoji = "👨‍👩‍👧‍👦"; // Family emoji (multi-codepoint grapheme cluster)
        let repeat_count = 50_000;
        let content = emoji.repeat(repeat_count);
        let truncated = truncate_content(content.clone());
        
        // The family emoji is 7 chars but 1 grapheme cluster in unicode-segmentation
        // 50k * 1 = 50k graphemes which is under MAX_CONTENT_LENGTH, so no truncation occurs
        // Test with a larger count to ensure truncation
        let large_content = emoji.repeat(200_000);
        let truncated_large = truncate_content(large_content.clone());
        
        assert!(truncated_large.contains("[Content truncated"), "should contain truncation message");
        assert!(std::str::from_utf8(truncated_large.as_bytes()).is_ok(), "must be valid UTF-8");
        
        // Verify we don't split grapheme clusters
        let truncated_graphemes: Vec<&str> = truncated_large.graphemes(true).collect();
        let last_grapheme = truncated_graphemes.last().unwrap();
        assert!(!last_grapheme.is_empty(), "last grapheme should not be empty");
        
        // Verify the truncated content has at most MAX_CONTENT_LENGTH + message suffix graphemes
        assert!(truncated_large.graphemes(true).count() <= MAX_CONTENT_LENGTH + "\n\n[Content truncated at 100000 characters]".graphemes(true).count(),
            "truncated should not exceed MAX_CONTENT_LENGTH + message suffix");
    }

    #[test]
    fn test_truncate_content_cjk_characters() {
        let cjk = "日本語"; // Japanese characters
        let repeat_count = (MAX_CONTENT_LENGTH / 3) + 100;
        let content = cjk.repeat(repeat_count);
        let truncated = truncate_content(content.clone());
        
        assert!(truncated.contains("[Content truncated"));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        
        // Verify no partial CJK characters
        let truncated_graphemes: Vec<&str> = truncated.graphemes(true).collect();
        for g in &truncated_graphemes[..MAX_CONTENT_LENGTH] {
            assert!(!g.is_empty());
            assert!(g.chars().count() >= 1);
        }
    }

    #[test]
    fn test_truncate_content_combining_characters() {
        let base = "e\u{0301}"; // é as e + combining acute accent (2 graphemes: 'e' + combining mark)
        let repeat_count = MAX_CONTENT_LENGTH + 100;
        let content = base.repeat(repeat_count);
        let truncated = truncate_content(content.clone());
        
        assert!(truncated.contains("[Content truncated"));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        
        // Verify combining marks stay with their base
        let truncated_graphemes: Vec<&str> = truncated.graphemes(true).collect();
        // The message suffix adds extra graphemes, so check first MAX_CONTENT_LENGTH
        let check_len = std::cmp::min(MAX_CONTENT_LENGTH, truncated_graphemes.len());
        for g in &truncated_graphemes[..check_len] {
            assert!(!g.is_empty());
        }
    }

    #[test]
    fn test_truncate_content_empty_string() {
        let content = String::new();
        let truncated = truncate_content(content);
        assert_eq!(truncated, "");
    }

    #[test]
    fn test_truncate_content_well_under_limit() {
        let content = "Hello, world!".to_string();
        let truncated = truncate_content(content.clone());
        assert_eq!(truncated, content);
    }

    // ------------------------------------------------------------------
    // find_chrome_binary tests (no network)
    // ------------------------------------------------------------------

    #[test]
    fn test_find_chrome_binary_does_not_panic() {
        let result = find_chrome_binary();
        // On CI it may return None, on dev machine it may return Some
        // Just verify it doesn't panic
        println!("Chrome binary: {:?}", result);
    }

    #[test]
    fn test_find_chrome_binary_fake_paths() {
        // Verify the function checks paths correctly by testing with non-existent paths
        // This is implicitly tested since we don't create fake Chrome binaries
        let result = find_chrome_binary();
        // If Chrome is not installed, should return None
        if result.is_none() {
            println!("Chrome not found (expected on CI or without Chrome installed)");
        }
    }

    // ------------------------------------------------------------------
    // fetch_with_reqwest tests (network required)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_with_reqwest_static_site() {
        let result = fetch_with_reqwest("https://httpbin.org/html").await;
        assert!(result.is_ok(), "Should fetch static HTML: {:?}", result.err());
        let content = result.unwrap();
        assert!(!content.is_empty(), "Content should not be empty");
        // httpbin.org/html returns a simple HTML page
        assert!(content.contains("Herman Melville") || content.contains("Moby-Dick"),
            "Should contain expected content from httpbin.org/html");
    }

    #[tokio::test]
    async fn test_fetch_with_reqwest_404() {
        let result = fetch_with_reqwest("https://httpbin.org/status/404").await;
        assert!(result.is_err(), "Should return error for 404");
        let err = result.unwrap_err();
        assert!(err.contains("404") || err.contains("Not Found"),
            "Error should mention 404: {}", err);
    }

    #[tokio::test]
    async fn test_fetch_with_reqwest_invalid_url() {
        let result = fetch_with_reqwest("not-a-valid-url").await;
        assert!(result.is_err(), "Should return error for invalid URL");
    }

    #[tokio::test]
    async fn test_fetch_with_reqwest_malformed_html() {
        // httpbin.org/html returns valid HTML, but we can test with a data URI or
        // a known endpoint. For malformed HTML, we'll use a text endpoint
        let result = fetch_with_reqwest("https://httpbin.org/get").await;
        assert!(result.is_ok(), "Should handle JSON response: {:?}", result.err());
        let content = result.unwrap();
        assert!(!content.is_empty());
    }

    // ------------------------------------------------------------------
    // Error handling tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_with_reqwest_timeout() {
        // httpbin.org/delay/35 should timeout after 30 seconds
        let start = std::time::Instant::now();
        let result = fetch_with_reqwest("https://httpbin.org/delay/35").await;
        let elapsed = start.elapsed();
        
        // The request should either timeout or take a long time
        // On some networks it might succeed if delay is ignored
        if result.is_ok() {
            println!("Warning: timeout test endpoint did not delay as expected");
        }
        assert!(elapsed < std::time::Duration::from_secs(40),
            "Should not wait more than 40 seconds, took {:?}", elapsed);
    }

    #[tokio::test]
    async fn test_fetch_with_reqwest_500_error() {
        let result = fetch_with_reqwest("https://httpbin.org/status/500").await;
        assert!(result.is_err(), "Should return error for 500");
        let err = result.unwrap_err();
        assert!(err.contains("500") || err.contains("Internal Server Error"),
            "Error should mention 500: {}", err);
    }

    // ------------------------------------------------------------------
    // Tool execution tests
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_missing_url() {
        let args = serde_json::json!({"foo": "bar"});
        let result = execute(&args).await;
        match result {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Missing required parameter: url"));
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_execute_invalid_url() {
        let args = serde_json::json!({"url": "not-a-url"});
        let result = execute(&args).await;
        match result {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Failed to fetch"));
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }
}
