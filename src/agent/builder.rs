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
use crate::trajectory::SharedTrajectoryRecorder;

/// Configuration for building an AgentLoop.
pub struct AgentLoopBuilder {
    pub system_prompt: String,
    pub session_manager: Arc<SessionManager>,
    pub session_id: Option<crate::session::SessionId>,
    pub token_budget: usize,
    pub with_agent_tool: bool,
    pub recorder: Option<SharedTrajectoryRecorder>,
    pub workspace: Option<Arc<crate::workspace::Workspace>>,
    pub mindset: Option<Arc<crate::mindset::Mindset>>,
    pub channel_processors: std::collections::HashMap<String, std::sync::Arc<dyn crate::channel::processor::ChannelProcessor>>,
    pub guardrails: Option<crate::guardrail::GuardrailChain>,
    pub external_cwd: Option<std::path::PathBuf>,
    pub interaction_queue: Option<Arc<crate::agent::external::interaction_queue::InteractionQueue>>,
}

impl AgentLoopBuilder {
    pub fn new(system_prompt: String, session_manager: Arc<SessionManager>) -> Self {
        Self {
            system_prompt,
            session_manager,
            session_id: None,
            token_budget: 153_600,
            with_agent_tool: true,
            recorder: None,
            workspace: None,
            mindset: None,
            channel_processors: std::collections::HashMap::new(),
            guardrails: None,
            external_cwd: None,
            interaction_queue: None,
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

    pub fn with_recorder(mut self, recorder: SharedTrajectoryRecorder) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn with_workspace(mut self, workspace: Arc<crate::workspace::Workspace>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_mindset(mut self, mindset: Arc<crate::mindset::Mindset>) -> Self {
        self.mindset = Some(mindset);
        self
    }

    pub fn with_channel_processor(
        mut self,
        name: impl Into<String>,
        processor: std::sync::Arc<dyn crate::channel::processor::ChannelProcessor>,
    ) -> Self {
        self.channel_processors.insert(name.into(), processor);
        self
    }

    pub fn with_guardrails(mut self, guardrails: crate::guardrail::GuardrailChain) -> Self {
        self.guardrails = Some(guardrails);
        self
    }

    pub fn with_external_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.external_cwd = Some(cwd);
        self
    }

    pub fn with_interaction_queue(
        mut self,
        queue: Arc<crate::agent::external::interaction_queue::InteractionQueue>,
    ) -> Self {
        self.interaction_queue = Some(queue);
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

            if let Some(ref ws) = self.workspace {
                r.register(Box::new(crate::tool::project::ProjectTool::new(ws.clone())));
                r.register(Box::new(crate::tool::task::TaskTool::new(ws.clone())));
            }

            for (_, processor) in &self.channel_processors {
                for tool in processor.tools() {
                    r.register(tool);
                }
            }

            if let Some(ref queue) = self.interaction_queue {
                r.register(Box::new(crate::tool::interaction_respond::InteractionRespondTool::new(queue.clone())));
            }

            if self.with_agent_tool {
                let subagent_registries = build_subagent_registries(&client, &self.workspace, &self.mindset);
                let mut agent_tool = AgentTool::new(
                    client.clone(),
                    subagent_registries,
                    self.system_prompt.clone(),
                    context_tx.clone(),
                    Some(self.session_manager.clone()),
                    Some(session_id.clone()),
                ).with_external_cwd(self.external_cwd.clone());

                if let Some(ref queue) = self.interaction_queue {
                    let (response_tx, _response_rx) = tokio::sync::mpsc::channel(32);
                    let (notify_tx, _notify_rx) = tokio::sync::mpsc::channel(32);
                    agent_tool = agent_tool.with_interaction_queue(
                        queue.clone(),
                        response_tx,
                        notify_tx,
                    );
                }

                if let Some(ref recorder) = self.recorder {
                    agent_tool = agent_tool.with_recorder(recorder.clone());
                }

                r.register(Box::new(agent_tool));
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
            recorder: self.recorder,
            tool_results_dir: Some(tool_results_dir),
            token_budget: self.token_budget,
            cancel: cancel.clone(),
            compaction: Box::new(EvictOldestTurns),
            context_tx: Some(context_tx.clone()),
            channel_processors: self.channel_processors,
            guardrails: self.guardrails.unwrap_or_else(crate::guardrail::GuardrailChain::new),
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

    #[tokio::test]
    async fn test_builder_with_recorder() {
        let client = dummy_client();
        let (mgr, session_id) = temp_manager();
        let recorder = std::sync::Arc::new(crate::trajectory::MemoryTrajectoryRecorder::new());
        let shared: crate::trajectory::SharedTrajectoryRecorder = recorder.clone();
        let builder = AgentLoopBuilder::new("Test".into(), mgr.clone())
            .with_session_id(session_id)
            .with_recorder(shared);

        let (_, _, _) = builder.build(client).await;

        let events = recorder.events();
        assert!(!events.is_empty(), "recorder should have at least one event");
        assert_eq!(events[0].event_type, "agent.started");

        let _ = std::fs::remove_dir_all(&mgr.home_dir);
    }
}
