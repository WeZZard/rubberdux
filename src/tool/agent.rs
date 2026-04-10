use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::agent::runtime::subagent::{spawn_subagent, ContextEvent};
use crate::provider::moonshot::MoonshotClient;
use crate::provider::moonshot::tool::ToolDefinition;
use crate::tool::ToolRegistry;

use super::ToolOutcome;

/// Tool that spawns a subagent with its own LLM loop to handle
/// complex, multi-step tasks autonomously.
pub struct AgentTool {
    client: Arc<MoonshotClient>,
    registry: Arc<ToolRegistry>,
    system_prompt: String,
    context_tx: broadcast::Sender<ContextEvent>,
}

impl AgentTool {
    pub fn new(
        client: Arc<MoonshotClient>,
        registry: Arc<ToolRegistry>,
        system_prompt: String,
        context_tx: broadcast::Sender<ContextEvent>,
    ) -> Self {
        Self {
            client,
            registry,
            system_prompt,
            context_tx,
        }
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

            let prompt = match args["prompt"].as_str() {
                Some(p) if !p.is_empty() => p.to_owned(),
                _ => {
                    return ToolOutcome::Immediate {
                        content: "Missing or empty 'prompt' parameter".into(),
                        is_error: true,
                    };
                }
            };

            let task_id = super::generate_task_id();
            let context_rx = self.context_tx.subscribe();

            log::info!("Spawning subagent {} for: {}", task_id, &prompt[..prompt.len().min(100)]);

            let handle = spawn_subagent(
                task_id,
                self.client.clone(),
                self.system_prompt.clone(),
                prompt,
                self.registry.clone(),
                context_rx,
            );

            ToolOutcome::Subagent { handle }
        })
    }
}
