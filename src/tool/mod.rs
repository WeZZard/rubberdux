pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod web_fetch;
pub mod write;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::provider::moonshot::tool::ToolDefinition;

// ---------------------------------------------------------------------------
// ToolOutcome — raw domain result from tool execution
// ---------------------------------------------------------------------------

/// Result delivered by a background task through its oneshot channel.
pub struct BackgroundTaskResult {
    pub task_id: String,
    pub content: String,
}

/// Raw outcome from tool execution, before provider-specific formatting.
pub enum ToolOutcome {
    /// Immediate result with raw content.
    Immediate { content: String, is_error: bool },
    /// Task dispatched to the background.
    Background {
        task_id: String,
        output_path: PathBuf,
        receiver: tokio::sync::oneshot::Receiver<BackgroundTaskResult>,
    },
}

/// Default formatting for ToolOutcome → Message::Tool content.
/// Sensible default: no file paths exposed, "you will be notified" for background tasks.
pub fn format_tool_outcome(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Immediate { content, .. } => content.clone(),
        ToolOutcome::Background { task_id, .. } => format!(
            "Background task {} is running. The result will be delivered \
             to you automatically when ready. Proceed with your response \
             to the user.",
            task_id
        ),
    }
}

/// Loads default tool definitions from embedded JSON files, keyed by tool name.
pub fn default_tool_definitions() -> BTreeMap<String, ToolDefinition> {
    const JSONS: &[&str] = &[
        include_str!("bash.json"),
        include_str!("web_fetch.json"),
        include_str!("read_file.json"),
        include_str!("write_file.json"),
        include_str!("edit_file.json"),
        include_str!("glob.json"),
        include_str!("grep.json"),
    ];

    JSONS
        .iter()
        .filter_map(|json| serde_json::from_str::<ToolDefinition>(json).ok())
        .map(|def| (def.function.name.clone(), def))
        .collect()
}

// ---------------------------------------------------------------------------
// Legacy ToolResult — used by tools that haven't migrated to ToolOutcome yet
// ---------------------------------------------------------------------------

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Generic tool execution
// ---------------------------------------------------------------------------

pub async fn execute_tool(name: &str, arguments: &str) -> ToolOutcome {
    let args: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => {
            return ToolOutcome::Immediate {
                content: format!("Failed to parse tool arguments: {}", e),
                is_error: true,
            }
        }
    };

    match name {
        "bash" => bash::execute(&args).await,
        "web_fetch" => web_fetch::execute(&args).await,
        "read_file" => {
            let r = read::execute(&args).await;
            ToolOutcome::Immediate { content: r.content, is_error: r.is_error }
        }
        "write_file" => {
            let r = write::execute(&args).await;
            ToolOutcome::Immediate { content: r.content, is_error: r.is_error }
        }
        "edit_file" => {
            let r = edit::execute(&args).await;
            ToolOutcome::Immediate { content: r.content, is_error: r.is_error }
        }
        "glob" => {
            let r = glob::execute(&args).await;
            ToolOutcome::Immediate { content: r.content, is_error: r.is_error }
        }
        "grep" => {
            let r = grep::execute(&args).await;
            ToolOutcome::Immediate { content: r.content, is_error: r.is_error }
        }
        _ => ToolOutcome::Immediate {
            content: format!("Unknown tool: {}", name),
            is_error: true,
        },
    }
}

pub(crate) fn generate_task_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:x}", ts & 0xFFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let outcome = execute_tool("nonexistent", "{}").await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Unknown tool"));
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[test]
    fn test_format_tool_outcome_immediate() {
        let outcome = ToolOutcome::Immediate {
            content: "file contents here".into(),
            is_error: false,
        };
        assert_eq!(format_tool_outcome(&outcome), "file contents here");
    }

    #[test]
    fn test_format_tool_outcome_background() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let outcome = ToolOutcome::Background {
            task_id: "bg_abc123".into(),
            output_path: PathBuf::from("./sessions/tasks/bg_abc123.output"),
            receiver: rx,
        };
        let formatted = format_tool_outcome(&outcome);
        assert!(formatted.contains("bg_abc123"), "should contain task_id");
        assert!(!formatted.contains("sessions/tasks"), "should NOT contain file path");
        assert!(formatted.contains("delivered"), "should tell model result will be delivered");
    }

    #[test]
    fn test_default_tool_definitions() {
        let defs = default_tool_definitions();
        assert_eq!(defs.len(), 7);
        assert!(defs.contains_key("bash"));
        assert!(defs.contains_key("web_fetch"));
        assert!(defs.contains_key("read_file"));
    }
}
