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

    let offset = args["offset"].as_u64().unwrap_or(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

    match std::fs::read_to_string(file_path) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            let start = if offset > 0 { offset - 1 } else { 0 };
            let end = (start + limit).min(lines.len());

            let numbered: Vec<String> = lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{}\t{}", start + i + 1, line))
                .collect();

            ToolResult {
                content: numbered.join("\n"),
                is_error: false,
            }
        }
        Err(e) => ToolResult {
            content: format!("Failed to read file {}: {}", file_path, e),
            is_error: true,
        },
    }
}
