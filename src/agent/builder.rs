use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::agent::runtime::compaction::EvictOldestTurns;
use crate::agent::runtime::port::InputPort;
use crate::agent::runtime::subagent::ContextEvent;
use crate::provider::moonshot::MoonshotClient;
use crate::tool::ToolRegistry;
use crate::tool::agent::{build_subagent_registries, AgentTool};
use crate::tool::edit::EditFileTool;
use crate::tool::glob::GlobTool;
use crate::tool::grep::GrepTool;
use crate::tool::read::ReadFileTool;
use crate::tool::write::WriteFileTool;
use crate::provider::moonshot::tool::bash::MoonshotBashTool;
use crate::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
use crate::provider::moonshot::tool::web_search::WebSearchTool;

/// Configuration for building an AgentLoop.
pub struct AgentLoopBuilder {
    pub system_prompt: String,
    pub data_dir: PathBuf,
    pub session_file: String,
    pub token_budget: usize,
    pub with_agent_tool: bool,
}

impl AgentLoopBuilder {
    pub fn new(system_prompt: String, data_dir: PathBuf) -> Self {
        Self {
            system_prompt,
            data_dir,
            session_file: "session.jsonl".to_string(),
            token_budget: 153_600,
            with_agent_tool: true,
        }
    }

    pub fn with_session_file(mut self, file: String) -> Self {
        self.session_file = file;
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
    pub fn build(self, client: Arc<MoonshotClient>) -> (AgentLoop, InputPort, broadcast::Sender<ContextEvent>) {
        let sessions_dir = self.data_dir.join("sessions");
        let tool_results_dir = self.data_dir.join("tool-results");
        let subagents_dir = self.data_dir.join("subagents");

        let _ = std::fs::create_dir_all(&sessions_dir);
        let _ = std::fs::create_dir_all(&tool_results_dir);
        let _ = std::fs::create_dir_all(&subagents_dir);

        let session_path = sessions_dir.join(&self.session_file);

        let (context_tx, _) = broadcast::channel::<ContextEvent>(64);
        let cancel = CancellationToken::new();

        let last_user_query = Arc::new(std::sync::RwLock::new(String::new()));

        let registry = {
            let mut r = ToolRegistry::new();
            r.register(Box::new(MoonshotBashTool::new()));
            r.register(Box::new(MoonshotWebFetchTool::new()));
            r.register(Box::new(ReadFileTool));
            r.register(Box::new(WriteFileTool));
            r.register(Box::new(EditFileTool));
            r.register(Box::new(GlobTool));
            r.register(Box::new(GrepTool));
            r.register(Box::new(WebSearchTool::new(
                client.clone(),
                last_user_query.clone(),
            )));

            if self.with_agent_tool {
                let subagent_registries = build_subagent_registries(&client, &last_user_query);
                r.register(Box::new(AgentTool::new(
                    client.clone(),
                    subagent_registries,
                    self.system_prompt.clone(),
                    context_tx.clone(),
                    Some(subagents_dir),
                )));
            }

            r
        };

        let config = AgentLoopConfig {
            client,
            registry: Arc::new(registry),
            system_prompt: self.system_prompt,
            session_path: Some(session_path),
            tool_results_dir: Some(tool_results_dir),
            token_budget: self.token_budget,
            cancel: cancel.clone(),
            compaction: Box::new(EvictOldestTurns),
            context_tx: Some(context_tx.clone()),
        };

        let (agent_loop, input_port) = AgentLoop::new(config);

        (agent_loop, input_port, context_tx)
    }
}
