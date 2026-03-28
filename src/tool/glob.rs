use super::ToolResult;

pub async fn execute(args: &serde_json::Value) -> ToolResult {
    let pattern = match args["pattern"].as_str() {
        Some(p) => p,
        None => {
            return ToolResult {
                content: "Missing required parameter: pattern".into(),
                is_error: true,
            }
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
        .arg(format!("find {} -path '{}' 2>/dev/null | head -250", base_path, full_pattern))
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
                    return ToolResult {
                        content: format!("Failed to execute glob: {}", e),
                        is_error: true,
                    }
                }
            }
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        ToolResult {
            content: format!("No files matching pattern: {}", pattern),
            is_error: false,
        }
    } else {
        ToolResult {
            content: stdout.trim().to_owned(),
            is_error: false,
        }
    }
}
