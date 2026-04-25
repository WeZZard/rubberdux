use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::agent::runtime::compaction::EvictOldestTurns;
use crate::agent::runtime::port::InputPort;
use crate::agent::runtime::subagent::ContextEvent;
use crate::provider::moonshot::MoonshotClient;
use crate::provider::moonshot::tool::bash::MoonshotBashTool;
use crate::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
use crate::provider::moonshot::tool::web_search::WebSearchTool;
use crate::session::SessionManager;
use crate::tool::ToolRegistry;
use crate::tool::agent::{AgentTool, build_subagent_registries};
use crate::tool::edit::EditFileTool;
use crate::tool::glob::GlobTool;
use crate::tool::grep::GrepTool;
use crate::tool::read::ReadFileTool;
use crate::tool::write::WriteFileTool;

/// Configuration for building an AgentLoop.
pub struct AgentLoopBuilder {
    pub system_prompt: String,
    pub session_manager: Arc<SessionManager>,
    pub session_id: Option<crate::session::SessionId>,
    pub token_budget: usize,
    pub with_agent_tool: bool,
}

impl AgentLoopBuilder {
    pub fn new(system_prompt: String, session_manager: Arc<SessionManager>) -> Self {
        Self {
            system_prompt,
            session_manager,
            session_id: None,
            token_budget: 153_600,
            with_agent_tool: true,
        }
    }

    pub fn with_session_id(mut self, session_id: crate::session::SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_token_budget(mut self, budget: usize) -> Self {
        self.token_budget = budget;
        self
    }

    pub fn with_agent_tool(mut self, enabled: bool) -> Self {
        self.with_agent_tool = enabled;
        self
    }

    /// Build the AgentLoop and return it along with its input port and context broadcaster.
    pub async fn build(
        self,
        client: Arc<MoonshotClient>,
    ) -> (AgentLoop, InputPort, broadcast::Sender<ContextEvent>) {
        let session_id = self
            .session_id
            .expect("session_id must be set before building");
        let main_agent_dir = self.session_manager.main_agent_dir(&session_id);
        let tool_results_dir = main_agent_dir.join("tool_results");

        let session_path = main_agent_dir.join("session.jsonl");

        let (context_tx, _) = broadcast::channel::<ContextEvent>(64);
        let cancel = CancellationToken::new();

        let registry = {
            let mut r = ToolRegistry::new();
            r.register(Box::new(MoonshotBashTool::new()));
            r.register(Box::new(MoonshotWebFetchTool::new()));
            r.register(Box::new(ReadFileTool));
            r.register(Box::new(WriteFileTool));
            r.register(Box::new(EditFileTool));
            r.register(Box::new(GlobTool));
            r.register(Box::new(GrepTool));
            r.register(Box::new(WebSearchTool::new(client.clone())));

            if self.with_agent_tool {
                let subagent_registries = build_subagent_registries(&client);
                r.register(Box::new(AgentTool::new(
                    client.clone(),
                    subagent_registries,
                    self.system_prompt.clone(),
                    context_tx.clone(),
                    Some(self.session_manager.clone()),
                    Some(session_id.clone()),
                )));
            }

            r
        };

        let config = AgentLoopConfig {
            client,
            registry: Arc::new(registry),
            system_prompt: self.system_prompt,
            session_path: Some(session_path),
            session_id: Some(session_id.to_string()),
            agent_id: Some("main".into()),
            recorder: None,
            tool_results_dir: Some(tool_results_dir),
            token_budget: self.token_budget,
            cancel: cancel.clone(),
            compaction: Box::new(EvictOldestTurns),
            context_tx: Some(context_tx.clone()),
        };

        let (agent_loop, input_port) = AgentLoop::new(config).await;

        (agent_loop, input_port, context_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;

    fn dummy_client() -> Arc<MoonshotClient> {
        Arc::new(MoonshotClient::new(
            reqwest::Client::new(),
            "http://localhost:0".into(),
            "test-key".into(),
            "test-model".into(),
        ))
    }

    fn temp_manager() -> (Arc<SessionManager>, crate::session::SessionId) {
        let home = tempfile::tempdir().unwrap().into_path();
        let mgr = Arc::new(SessionManager {
            home_dir: home.clone(),
            sessions_dir: home.join("sessions"),
            latest_link: home.join("latest"),
        });
        let (session_id, _) = mgr.create_session("test-model".into()).unwrap();
        (mgr, session_id)
    }

    #[tokio::test]
    async fn test_builder_creates_agent_loop_happy_path() {
        let client = dummy_client();
        let (mgr, session_id) = temp_manager();
        let builder = AgentLoopBuilder::new("Test system prompt".into(), mgr.clone())
            .with_session_id(session_id.clone());

        let (agent_loop, input_port, context_tx) = builder.build(client).await;

        // Verify main agent dir was created
        assert!(
            mgr.main_agent_dir(&session_id).exists(),
            "main agent dir should exist"
        );
        assert!(
            mgr.main_agent_dir(&session_id)
                .join("tool_results")
                .exists()
        );

        // Clean up
        let _ = std::fs::remove_dir_all(&mgr.home_dir);
    }

    #[tokio::test]
    async fn test_builder_with_custom_token_budget() {
        let client = dummy_client();
        let (mgr, session_id) = temp_manager();
        let builder = AgentLoopBuilder::new("Test".into(), mgr.clone())
            .with_session_id(session_id)
            .with_token_budget(50_000);

        let (_, _, _) = builder.build(client).await;

        let _ = std::fs::remove_dir_all(&mgr.home_dir);
    }

    #[tokio::test]
    async fn test_builder_without_agent_tool() {
        let client = dummy_client();
        let (mgr, session_id) = temp_manager();
        let builder = AgentLoopBuilder::new("Test".into(), mgr.clone())
            .with_session_id(session_id)
            .with_agent_tool(false);

        let (_, _, _) = builder.build(client).await;

        let _ = std::fs::remove_dir_all(&mgr.home_dir);
    }

    #[tokio::test]
    async fn test_builder_handles_empty_system_prompt() {
        let client = dummy_client();
        let (mgr, session_id) = temp_manager();
        let builder = AgentLoopBuilder::new("".into(), mgr.clone()).with_session_id(session_id);

        let (_, _, _) = builder.build(client).await;

        let _ = std::fs::remove_dir_all(&mgr.home_dir);
    }

    #[tokio::test]
    async fn test_context_tx_can_subscribe() {
        let client = dummy_client();
        let (mgr, session_id) = temp_manager();
        let builder = AgentLoopBuilder::new("Test".into(), mgr.clone()).with_session_id(session_id);

        let (_, _, context_tx) = builder.build(client).await;

        let mut rx = context_tx.subscribe();
        context_tx.send(ContextEvent::Cancel).unwrap();

        let received = rx.try_recv();
        assert!(received.is_ok(), "Should receive context event");

        let _ = std::fs::remove_dir_all(&mgr.home_dir);
    }
}
