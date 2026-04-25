use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex, broadcast};

use crate::agent::runtime::subagent::{ContextEvent, spawn_subagent};
use crate::hardened_prompts::subagent_preamble;
use crate::provider::moonshot::MoonshotClient;
use crate::provider::moonshot::tool::ToolDefinition;
use crate::session::{AgentMetadata, SessionManager};
use crate::tool::{SubagentType, ToolRegistry};

use super::ToolOutcome;

/// Builds per-subagent-type tool registries.
///
/// Explore and Plan share a single read-only registry.
/// GeneralPurpose and ComputerUse get a full registry (no recursive `agent`).
pub fn build_subagent_registries(
    client: &Arc<MoonshotClient>,
) -> HashMap<SubagentType, Arc<ToolRegistry>> {
    use crate::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
    use crate::provider::moonshot::tool::web_search::WebSearchTool;
    use crate::tool::glob::GlobTool;
    use crate::tool::grep::GrepTool;
    use crate::tool::read::ReadFileTool;

    let readonly = Arc::new({
        let mut r = ToolRegistry::new();
        r.register(Box::new(GlobTool));
        r.register(Box::new(GrepTool));
        r.register(Box::new(ReadFileTool));
        r.register(Box::new(MoonshotWebFetchTool::new()));
        r.register(Box::new(WebSearchTool::new(client.clone())));
        r
    });

    let general_purpose = Arc::new({
        use crate::provider::moonshot::tool::bash::MoonshotBashTool;
        use crate::tool::edit::EditFileTool;
        use crate::tool::write::WriteFileTool;

        let mut r = ToolRegistry::new();
        r.register(Box::new(MoonshotBashTool::new()));
        r.register(Box::new(MoonshotWebFetchTool::new()));
        r.register(Box::new(ReadFileTool));
        r.register(Box::new(WriteFileTool));
        r.register(Box::new(EditFileTool));
        r.register(Box::new(GlobTool));
        r.register(Box::new(GrepTool));
        r.register(Box::new(WebSearchTool::new(client.clone())));
        r
    });

    let mut map = HashMap::new();
    map.insert(SubagentType::Explore, readonly.clone());
    map.insert(SubagentType::Plan, readonly);
    map.insert(SubagentType::GeneralPurpose, general_purpose.clone());
    map.insert(SubagentType::ComputerUse, general_purpose);
    map
}

/// Unified agent tool that routes to the correct execution strategy
/// based on `subagent_type`.
pub struct AgentTool {
    client: Arc<MoonshotClient>,
    registries: HashMap<SubagentType, Arc<ToolRegistry>>,
    base_system_prompt: String,
    context_tx: broadcast::Sender<ContextEvent>,
    /// Session manager for creating subagent directories.
    session_manager: Option<Arc<SessionManager>>,
    /// Current session ID for creating subagent directories.
    session_id: Option<crate::session::SessionId>,
    /// Host RPC writer for dispatching isolated computer-use VM agents.
    rpc_writer: Option<Arc<Mutex<OwnedWriteHalf>>>,
}

impl AgentTool {
    pub fn new(
        client: Arc<MoonshotClient>,
        registries: HashMap<SubagentType, Arc<ToolRegistry>>,
        base_system_prompt: String,
        context_tx: broadcast::Sender<ContextEvent>,
        session_manager: Option<Arc<SessionManager>>,
        session_id: Option<crate::session::SessionId>,
    ) -> Self {
        Self {
            client,
            registries,
            base_system_prompt,
            context_tx,
            session_manager,
            session_id,
            rpc_writer: None,
        }
    }

    pub fn with_rpc_writer(mut self, rpc_writer: Option<Arc<Mutex<OwnedWriteHalf>>>) -> Self {
        self.rpc_writer = rpc_writer;
        self
    }
}

impl super::Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn definition(&self) -> ToolDefinition {
        serde_json::from_str(include_str!("agent.json"))
            .expect("agent.json must be valid ToolDefinition")
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
                        content: format!("Failed to parse agent arguments: {}", e),
                        is_error: true,
                    };
                }
            };

            let subagent_type: SubagentType = match args.get("subagent_type") {
                Some(v) => match serde_json::from_value(v.clone()) {
                    Ok(t) => t,
                    Err(_) => {
                        return ToolOutcome::Immediate {
                            content: "Invalid 'subagent_type'. Must be one of: explore, plan, general_purpose, computer_use".into(),
                            is_error: true,
                        };
                    }
                },
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required 'subagent_type' parameter".into(),
                        is_error: true,
                    };
                }
            };

            let prompt = match args["prompt"].as_str() {
                Some(p) if !p.is_empty() => p.to_owned(),
                _ => {
                    return ToolOutcome::Immediate {
                        content: "Missing or empty 'prompt' parameter".into(),
                        is_error: true,
                    };
                }
            };

            if subagent_type == SubagentType::ComputerUse {
                if let Some(rpc_writer) = &self.rpc_writer {
                    let task_id = super::generate_task_id();
                    let msg = crate::protocol::AgentToHost::SpawnVM {
                        task_id: task_id.clone(),
                        prompt,
                        subagent_type: "computer_use".to_owned(),
                    };

                    let mut writer = rpc_writer.lock().await;
                    return match crate::protocol::write_message(&mut writer, &msg).await {
                        Ok(()) => ToolOutcome::Immediate {
                            content: format!("Computer-use VM agent {} dispatched.", task_id),
                            is_error: false,
                        },
                        Err(e) => ToolOutcome::Immediate {
                            content: format!("Failed to dispatch computer-use VM agent: {}", e),
                            is_error: true,
                        },
                    };
                }
            }

            let registry = match self.registries.get(&subagent_type) {
                Some(r) => r.clone(),
                None => {
                    return ToolOutcome::Immediate {
                        content: format!("No registry for subagent type {:?}", subagent_type),
                        is_error: true,
                    };
                }
            };

            let task_id = super::generate_task_id();
            let system_prompt = format!(
                "{}\n\n{}",
                subagent_preamble(subagent_type),
                self.base_system_prompt
            );

            log::info!(
                "Spawning {:?} subagent {} for: {}",
                subagent_type,
                task_id,
                &prompt[..prompt.len().min(100)]
            );

            // Persist subagent session and metadata using SessionManager
            let (subagent_session, subagent_tool_results) =
                if let (Some(mgr), Some(session_id)) = (&self.session_manager, &self.session_id) {
                    let metadata = AgentMetadata::for_subagent(
                        "kimi-for-coding".into(), // TODO: get actual model from client
                        task_id.clone(),
                        session_id.to_string(),
                        format!("{:?}", subagent_type),
                    );

                    match mgr.create_subagent_dir(session_id, &task_id, &metadata, &prompt) {
                        Ok(agent_dir) => {
                            let tool_results_dir = agent_dir.join("tool_results");
                            (
                                Some(agent_dir.join("session.jsonl")),
                                Some(tool_results_dir),
                            )
                        }
                        Err(e) => {
                            log::warn!("Failed to create subagent dir: {}", e);
                            (None, None)
                        }
                    }
                } else {
                    (None, None)
                };

            let context_rx = self.context_tx.subscribe();
            let handle = spawn_subagent(
                task_id,
                self.client.clone(),
                system_prompt,
                prompt,
                registry,
                context_rx,
                subagent_session,
                subagent_tool_results,
            );

            ToolOutcome::Subagent { handle }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;

    fn dummy_client() -> Arc<MoonshotClient> {
        Arc::new(MoonshotClient::new(
            reqwest::Client::new(),
            "http://localhost:0".into(),
            "test-key".into(),
            "test-model".into(),
        ))
    }

    fn dummy_registries() -> HashMap<SubagentType, Arc<ToolRegistry>> {
        let client = dummy_client();
        build_subagent_registries(&client)
    }

    fn dummy_agent_tool() -> AgentTool {
        let client = dummy_client();
        let registries = dummy_registries();
        let (context_tx, _) = broadcast::channel(4);
        AgentTool::new(
            client,
            registries,
            "test system prompt".into(),
            context_tx,
            None,
            None,
        )
    }

    // --- Registry construction tests ---

    #[test]
    fn test_registries_contain_all_types() {
        let registries = dummy_registries();
        assert!(registries.contains_key(&SubagentType::Explore));
        assert!(registries.contains_key(&SubagentType::Plan));
        assert!(registries.contains_key(&SubagentType::GeneralPurpose));
        assert!(registries.contains_key(&SubagentType::ComputerUse));
    }

    #[test]
    fn test_explore_plan_share_arc() {
        let registries = dummy_registries();
        let explore = registries.get(&SubagentType::Explore).unwrap();
        let plan = registries.get(&SubagentType::Plan).unwrap();
        assert!(Arc::ptr_eq(explore, plan));
    }

    #[test]
    fn test_readonly_registry_tools() {
        let registries = dummy_registries();
        let r = registries.get(&SubagentType::Explore).unwrap();
        let defs: Vec<String> = r
            .definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert_eq!(
            defs.len(),
            5,
            "readonly registry should have exactly 5 tools, got {:?}",
            defs
        );
        for expected in ["glob", "grep", "read_file", "web_fetch", "web_search"] {
            assert!(
                defs.contains(&expected.to_owned()),
                "readonly registry missing {}",
                expected
            );
        }
        for absent in ["bash", "write_file", "edit_file", "agent"] {
            assert!(
                !defs.contains(&absent.to_owned()),
                "readonly registry should not have {}",
                absent
            );
        }
    }

    #[test]
    fn test_gp_registry_tools() {
        let registries = dummy_registries();
        let r = registries.get(&SubagentType::GeneralPurpose).unwrap();
        let defs: Vec<String> = r
            .definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert_eq!(
            defs.len(),
            8,
            "gp registry should have exactly 8 tools, got {:?}",
            defs
        );
        for expected in [
            "bash",
            "web_fetch",
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "web_search",
        ] {
            assert!(
                defs.contains(&expected.to_owned()),
                "gp registry missing {}",
                expected
            );
        }
        assert!(
            !defs.contains(&"agent".to_owned()),
            "gp registry should not have agent"
        );
    }

    #[test]
    fn test_computer_use_registry_tools() {
        let registries = dummy_registries();
        let r = registries.get(&SubagentType::ComputerUse).unwrap();
        let defs: Vec<String> = r
            .definitions()
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert_eq!(
            defs.len(),
            8,
            "computer_use registry should have exactly 8 tools, got {:?}",
            defs
        );
        for expected in [
            "bash",
            "web_fetch",
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "web_search",
        ] {
            assert!(
                defs.contains(&expected.to_owned()),
                "computer_use registry missing {}",
                expected
            );
        }
        assert!(
            !defs.contains(&"agent".to_owned()),
            "computer_use registry should not have agent"
        );
    }

    // --- AgentTool::execute argument validation tests ---

    #[tokio::test]
    async fn test_missing_subagent_type() {
        let tool = dummy_agent_tool();
        let outcome = <AgentTool as Tool>::execute(&tool, r#"{"prompt":"x"}"#).await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(
                    content.contains("Missing"),
                    "expected 'Missing', got: {}",
                    content
                );
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_invalid_subagent_type() {
        let tool = dummy_agent_tool();
        let outcome =
            <AgentTool as Tool>::execute(&tool, r#"{"subagent_type":"bogus","prompt":"x"}"#).await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(
                    content.contains("Invalid"),
                    "expected 'Invalid', got: {}",
                    content
                );
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_missing_prompt() {
        let tool = dummy_agent_tool();
        let outcome = <AgentTool as Tool>::execute(&tool, r#"{"subagent_type":"explore"}"#).await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(
                    content.contains("prompt"),
                    "expected 'prompt', got: {}",
                    content
                );
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_empty_prompt() {
        let tool = dummy_agent_tool();
        let outcome =
            <AgentTool as Tool>::execute(&tool, r#"{"subagent_type":"explore","prompt":""}"#).await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(
                    content.contains("prompt"),
                    "expected 'prompt', got: {}",
                    content
                );
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_explore_returns_subagent() {
        let tool = dummy_agent_tool();
        let outcome =
            <AgentTool as Tool>::execute(&tool, r#"{"subagent_type":"explore","prompt":"find x"}"#)
                .await;
        match outcome {
            ToolOutcome::Subagent { handle } => {
                handle.cancel.cancel();
            }
            other => panic!(
                "Expected Subagent outcome, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn test_plan_returns_subagent() {
        let tool = dummy_agent_tool();
        let outcome =
            <AgentTool as Tool>::execute(&tool, r#"{"subagent_type":"plan","prompt":"plan x"}"#)
                .await;
        match outcome {
            ToolOutcome::Subagent { handle } => {
                handle.cancel.cancel();
            }
            other => panic!(
                "Expected Subagent outcome, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn test_malformed_json() {
        let tool = dummy_agent_tool();
        let outcome = <AgentTool as Tool>::execute(&tool, r#"{broken""#).await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(
                    content.contains("parse"),
                    "expected 'parse', got: {}",
                    content
                );
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_missing_registry_for_type() {
        let client = dummy_client();
        let (context_tx, _) = broadcast::channel(4);
        let tool = AgentTool::new(
            client,
            HashMap::new(), // empty registries
            "test system prompt".into(),
            context_tx,
            None,
            None,
        );
        let outcome =
            <AgentTool as Tool>::execute(&tool, r#"{"subagent_type":"explore","prompt":"find x"}"#)
                .await;
        match outcome {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(is_error);
                assert!(
                    content.contains("No registry"),
                    "expected 'No registry', got: {}",
                    content
                );
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }

    #[tokio::test]
    async fn test_gp_returns_subagent() {
        let tool = dummy_agent_tool();
        let outcome = <AgentTool as Tool>::execute(
            &tool,
            r#"{"subagent_type":"general_purpose","prompt":"do x"}"#,
        )
        .await;
        match outcome {
            ToolOutcome::Subagent { handle } => {
                handle.cancel.cancel();
            }
            other => panic!(
                "Expected Subagent outcome, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn test_computer_use_no_rpc_returns_subagent() {
        let tool = dummy_agent_tool();
        let outcome = <AgentTool as Tool>::execute(
            &tool,
            r#"{"subagent_type":"computer_use","prompt":"click ok"}"#,
        )
        .await;
        match outcome {
            ToolOutcome::Subagent { handle } => {
                handle.cancel.cancel();
            }
            other => panic!(
                "Expected Subagent outcome, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}
