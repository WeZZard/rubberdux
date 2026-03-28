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

    let old_string = match args["old_string"].as_str() {
        Some(s) => s,
        None => {
            return ToolResult {
                content: "Missing required parameter: old_string".into(),
                is_error: true,
            }
        }
    };

    let new_string = match args["new_string"].as_str() {
        Some(s) => s,
        None => {
            return ToolResult {
                content: "Missing required parameter: new_string".into(),
                is_error: true,
            }
        }
    };

    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult {
                content: format!("Failed to read file {}: {}", file_path, e),
                is_error: true,
            }
        }
    };

    let count = content.matches(old_string).count();

    if count == 0 {
        return ToolResult {
            content: format!("old_string not found in {}", file_path),
            is_error: true,
        };
    }

    if count > 1 {
        return ToolResult {
            content: format!(
                "old_string found {} times in {}. Must be unique. Provide more context.",
                count, file_path
            ),
            is_error: true,
        };
    }

    let new_content = content.replacen(old_string, new_string, 1);

    match std::fs::write(file_path, &new_content) {
        Ok(()) => ToolResult {
            content: format!("Successfully edited {}", file_path),
            is_error: false,
        },
        Err(e) => ToolResult {
            content: format!("Failed to write file {}: {}", file_path, e),
            is_error: true,
        },
    }
}
