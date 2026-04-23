pub mod agent;
pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod web_fetch;
pub mod write;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::provider::moonshot::tool::ToolDefinition;

/// Subagent execution strategy. Determines the tool registry and
/// execution environment for a spawned subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentType {
    /// Read-only. Code search, finding definitions, tracing call chains.
    Explore,
    /// Read-only. Design implementation plans, analyze tradeoffs.
    Plan,
    /// Full access. Write code, run builds, execute tests.
    GeneralPurpose,
    /// Isolated GUI automation. May open apps, move the mouse, and send keys.
    /// Runs inside a dedicated child VM for host safety.
    ComputerUse,
}

// ---------------------------------------------------------------------------
// Tool trait — unified abstraction for tool definition + execution
// ---------------------------------------------------------------------------

/// System-declared tool abstraction. Each tool encapsulates its
/// definition (what the model sees) and execution (how it runs).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;
}

/// Dynamic tool registry. Holds tool instances, provides unified dispatch.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn unregister(&mut self, name: &str) -> bool {
        let before = self.tools.len();
        self.tools.retain(|t| t.name() != name);
        self.tools.len() < before
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, name: &str, arguments: &str) -> ToolOutcome {
        match self.get(name) {
            Some(tool) => tool.execute(arguments).await,
            None => ToolOutcome::Immediate {
                content: format!("Unknown tool: {}", name),
                is_error: true,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// ToolOutcome — raw domain result from tool execution
// ---------------------------------------------------------------------------

/// Result delivered by a background task through its oneshot channel.
#[derive(Debug, Clone)]
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
    /// Subagent dispatched with its own LLM loop.
    Subagent {
        handle: crate::agent::runtime::subagent::SubagentHandle,
    },
}

/// Default formatting for ToolOutcome → Message::Tool content.
/// Sensible default: no file paths exposed, "you will be notified" for background tasks.
pub fn format_tool_outcome(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Immediate { content, .. } => content.clone(),
        ToolOutcome::Background { task_id, .. } => format!(
            "The tool launched successfully. Background task {} is processing \
             the request. The result will be delivered to you shortly. \
             Acknowledge the user's request and let them know the result \
             is on its way.",
            task_id
        ),
        ToolOutcome::Subagent { handle } => format!(
            "Subagent {} has been dispatched. This is foreground work — \
             the user is waiting for the result. When it arrives, you will \
             receive it as a message in this conversation. The user will \
             not see it directly; it is up to you to present it.",
            handle.task_id
        ),
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
    use std::future::Future;
    use std::pin::Pin;

    struct DummyTool;

    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new("dummy", "A dummy tool", serde_json::json!({}))
        }
        fn execute<'a>(
            &'a self,
            _arguments: &'a str,
        ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
            Box::pin(async {
                ToolOutcome::Immediate {
                    content: "dummy result".into(),
                    is_error: false,
                }
            })
        }
    }

    #[tokio::test]
    async fn test_registry_execute_known_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool));
        let outcome = registry.execute("dummy", "{}").await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error);
                assert_eq!(content, "dummy result");
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let outcome = registry.execute("nonexistent", "{}").await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(content.contains("Unknown tool"));
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[test]
    fn test_registry_definitions() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool));
        let defs = registry.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "dummy");
    }

    #[test]
    fn test_registry_unregister() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool));
        assert!(registry.unregister("dummy"));
        assert!(registry.definitions().is_empty());
        // Execute after unregister returns error
        let outcome =
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(registry.execute("dummy", "{}"));
        match outcome {
            ToolOutcome::Immediate { is_error, .. } => assert!(is_error),
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
    fn test_subagent_type_deserialize_explore() {
        let t: SubagentType = serde_json::from_str(r#""explore""#).unwrap();
        assert_eq!(t, SubagentType::Explore);
    }

    #[test]
    fn test_subagent_type_deserialize_plan() {
        let t: SubagentType = serde_json::from_str(r#""plan""#).unwrap();
        assert_eq!(t, SubagentType::Plan);
    }

    #[test]
    fn test_subagent_type_deserialize_general_purpose() {
        let t: SubagentType = serde_json::from_str(r#""general_purpose""#).unwrap();
        assert_eq!(t, SubagentType::GeneralPurpose);
    }

    #[test]
    fn test_subagent_type_deserialize_computer_use() {
        let t: SubagentType = serde_json::from_str(r#""computer_use""#).unwrap();
        assert_eq!(t, SubagentType::ComputerUse);
    }

    #[test]
    fn test_subagent_type_rejects_invalid() {
        let result = serde_json::from_str::<SubagentType>(r#""invalid""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_subagent_type_rejects_wrong_case() {
        let result = serde_json::from_str::<SubagentType>(r#""EXPLORE""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_subagent_type_rejects_empty_string() {
        let result = serde_json::from_str::<SubagentType>(r#""""#);
        assert!(result.is_err());
    }

    #[test]
    fn test_subagent_type_rejects_null() {
        let result = serde_json::from_str::<SubagentType>("null");
        assert!(result.is_err());
    }

    #[test]
    fn test_subagent_type_rejects_number() {
        let result = serde_json::from_str::<SubagentType>("0");
        assert!(result.is_err());
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
}
