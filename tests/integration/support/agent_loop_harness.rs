use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rubberdux::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
use rubberdux::agent::runtime::compaction::EvictOldestTurns;
use rubberdux::agent::runtime::port::{LoopEvent, LoopOutput};
use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};
use rubberdux::tool::ToolRegistry;

/// Test harness that drives `AgentLoop` directly, bypassing the Telegram channel layer.
/// Uses real LLM calls (MoonshotClient::from_env) and the full tool registry.
pub struct AgentLoopHarness {
    input_port: rubberdux::agent::runtime::port::InputPort,
    session_path: PathBuf,
    _handle: tokio::task::JoinHandle<()>,
}

pub struct MessageExchange {
    pub outputs: Vec<LoopOutput>,
    pub failure_reason: Option<String>,
}

impl AgentLoopHarness {
    pub async fn new(system_prompt: &str, session_path: PathBuf) -> Self {
        let client = Arc::new(MoonshotClient::from_env());
        let registry = build_tool_registry(client.clone());
        Self::new_with_registry(system_prompt, session_path, client, Arc::new(registry)).await
    }

    pub async fn new_with_registry(
        system_prompt: &str,
        session_path: PathBuf,
        client: Arc<MoonshotClient>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        let session_dir = session_path.parent().map(|p| p.to_path_buf());
        let tool_results_dir = session_dir.as_ref().map(|d| d.join("tool-results"));

        let config = AgentLoopConfig {
            client,
            registry,
            system_prompt: system_prompt.to_string(),
            session_path: Some(session_path.clone()),
            session_id: None,
            agent_id: Some("main".into()),
            recorder: None,
            tool_results_dir,
            token_budget: 100_000,
            cancel: CancellationToken::new(),
            compaction: Box::new(EvictOldestTurns),
            context_tx: None,
        };

        let (agent_loop, input_port) = AgentLoop::new(config).await;
        let handle = tokio::spawn(async move {
            agent_loop.run().await;
        });

        Self {
            input_port,
            session_path,
            _handle: handle,
        }
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// Send a user message and collect all `LoopOutput` responses
    /// until `is_final == true` or the timeout expires.
    pub async fn send_message(&self, text: &str, timeout: Duration) -> MessageExchange {
        let (reply_tx, mut reply_rx) = mpsc::channel::<LoopOutput>(32);

        let event = LoopEvent::UserMessage {
            message: Message::User {
                content: UserContent::Text(text.to_string()),
            },
            reply: Some(reply_tx),
            metadata: Some(Box::new(1i32)),
        };

        self.input_port
            .send(event)
            .await
            .expect("input port should be open");

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut failure_reason = None;

        loop {
            match tokio::time::timeout_at(deadline, reply_rx.recv()).await {
                Ok(Some(output)) => {
                    let is_final = output.is_final;
                    responses.push(output);
                    if is_final {
                        break;
                    }
                }
                Ok(None) => {
                    failure_reason =
                        Some("Reply channel closed before a final assistant response".to_string());
                    break;
                }
                Err(_) => {
                    failure_reason = Some(format!(
                        "Timed out after {} second(s) waiting for a final assistant response",
                        timeout.as_secs()
                    ));
                    break;
                }
            }
        }

        MessageExchange {
            outputs: responses,
            failure_reason,
        }
    }

    /// Send multiple user messages as a batch.  All but the last message are
    /// injected as `ContextUpdate` (added to history without triggering LLM
    /// processing).  The last message is sent as a normal `UserMessage` which
    /// triggers the LLM response.
    pub async fn send_messages_batch(
        &self,
        messages: &[String],
        timeout: Duration,
    ) -> MessageExchange {
        assert!(
            !messages.is_empty(),
            "batch must contain at least one message"
        );

        // Send all but the last as context updates.
        for text in &messages[..messages.len() - 1] {
            let event = LoopEvent::ContextUpdate(Message::User {
                content: UserContent::Text(text.clone()),
            });
            self.input_port
                .send(event)
                .await
                .expect("input port should be open");
        }

        // Send the last message as a normal user message to trigger LLM.
        self.send_message(&messages[messages.len() - 1], timeout)
            .await
    }
}

/// Build the full tool registry the same way production does.
fn build_tool_registry(client: Arc<MoonshotClient>) -> ToolRegistry {
    use rubberdux::provider::moonshot::tool::bash::MoonshotBashTool;
    use rubberdux::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
    use rubberdux::provider::moonshot::tool::web_search::WebSearchTool;
    use rubberdux::tool::agent::{AgentTool, build_subagent_registries};
    use rubberdux::tool::edit::EditFileTool;
    use rubberdux::tool::glob::GlobTool;
    use rubberdux::tool::grep::GrepTool;
    use rubberdux::tool::read::ReadFileTool;
    use rubberdux::tool::write::WriteFileTool;

    let mut r = ToolRegistry::new();
    r.register(Box::new(MoonshotBashTool::new()));
    r.register(Box::new(MoonshotWebFetchTool::new()));
    r.register(Box::new(ReadFileTool));
    r.register(Box::new(WriteFileTool));
    r.register(Box::new(EditFileTool));
    r.register(Box::new(GlobTool));
    r.register(Box::new(GrepTool));
    r.register(Box::new(WebSearchTool::new(client.clone())));

    let subagent_registries = build_subagent_registries(&client);
    r.register(Box::new(AgentTool::new(
        client.clone(),
        subagent_registries,
        String::new(), // system_prompt — will be overridden by AgentLoopConfig
        tokio::sync::broadcast::channel::<rubberdux::agent::runtime::subagent::ContextEvent>(64).0,
        None,
        None,
    )));

    r
}
