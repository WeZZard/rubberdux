use super::ToolResult;

pub async fn execute(args: &serde_json::Value) -> ToolResult {
    let file_path = match args["file_path"].as_str() {
        Some(p) => p,
        None => {
            return ToolResult {
                content: "Missing required parameter: file_path".into(),
                is_error: true,
            }
        }
    };

    let content = match args["content"].as_str() {
        Some(c) => c,
        None => {
            return ToolResult {
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
        Ok(()) => ToolResult {
            content: format!("Successfully wrote {} bytes to {}", content.len(), file_path),
            is_error: false,
        },
        Err(e) => ToolResult {
            content: format!("Failed to write file {}: {}", file_path, e),
            is_error: true,
        },
    }
}
