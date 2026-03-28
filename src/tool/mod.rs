pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod web_fetch;
pub mod write;

use std::path::{Path, PathBuf};

use crate::provider::moonshot::tool::ToolDefinition;

const DEFAULT_TOOLS_DIR: &str = "./tools";

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

pub fn tools_dir() -> PathBuf {
    std::env::var("RUBBERDUX_TOOLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TOOLS_DIR))
}

pub fn load_tool_definitions(dir: &Path) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            log::error!("Failed to read tools directory {:?}: {}", dir, e);
            return definitions;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<ToolDefinition>(&content) {
                Ok(def) => {
                    log::info!("Loaded tool definition: {} from {:?}", def.function.name, path);
                    definitions.push(def);
                }
                Err(e) => log::warn!("Failed to parse tool definition {:?}: {}", path, e),
            },
            Err(e) => log::warn!("Failed to read tool definition {:?}: {}", path, e),
        }
    }

    definitions.sort_by(|a, b| a.function.name.cmp(&b.function.name));
    log::info!("Loaded {} tool definitions", definitions.len());
    definitions
}

/// Returns true if the tool is a Kimi builtin function (name starts with $).
/// Builtin tools are executed server-side — we echo the arguments back as the result.
pub fn is_builtin_tool(name: &str) -> bool {
    name.starts_with('$')
}

pub async fn execute_tool(name: &str, arguments: &str) -> ToolResult {
    // Kimi builtin functions: echo arguments back for server-side execution
    if is_builtin_tool(name) {
        return ToolResult {
            content: arguments.to_owned(),
            is_error: false,
        };
    }

    let args: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => {
            return ToolResult {
                content: format!("Failed to parse tool arguments: {}", e),
                is_error: true,
            }
        }
    };

    match name {
        "bash" => bash::execute(&args).await,
        "read_file" => read::execute(&args).await,
        "write_file" => write::execute(&args).await,
        "edit_file" => edit::execute(&args).await,
        "glob" => glob::execute(&args).await,
        "grep" => grep::execute(&args).await,
        "web_fetch" => web_fetch::execute(&args).await,
        _ => ToolResult {
            content: format!("Unknown tool: {}", name),
            is_error: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_tool_definitions() {
        let defs = load_tool_definitions(Path::new("./tools"));
        assert_eq!(defs.len(), 7); // 7 local tools (web_search is now a platform builtin)
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let result = execute_tool("nonexistent", "{}").await;
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool"));
    }
}
