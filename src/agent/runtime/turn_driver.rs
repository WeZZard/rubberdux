use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::entry::EntryHistory;
use crate::error::Error;
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::provider::moonshot::tool::ToolCall;
use crate::tool::{BackgroundTaskResult, ToolOutcome, ToolRegistry};

// ---------------------------------------------------------------------------
// TurnOutcome — result of driving one LLM turn
// ---------------------------------------------------------------------------

pub enum TurnOutcome {
    /// Model generated text response with no tool calls.
    Text { text: String, entry_id: usize },

    /// Model generated tool calls (may include text).
    /// background_tasks contains IDs of async tasks to wait for.
    Tools {
        text: String,
        entry_id: usize,
        background_tasks: Vec<String>,
    },

    /// Something went wrong calling the model.
    Failed { error: Error },
}

// ---------------------------------------------------------------------------
// TurnDriver — stateless component that drives one LLM turn
// ---------------------------------------------------------------------------

pub struct TurnDriver {
    client: Arc<MoonshotClient>,
    registry: Arc<ToolRegistry>,
    tool_results_dir: Option<PathBuf>,
}

impl TurnDriver {
    pub fn new(
        client: Arc<MoonshotClient>,
        registry: Arc<ToolRegistry>,
        tool_results_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            registry,
            tool_results_dir,
        }
    }

    /// Drive one turn: call LLM, execute tools, return outcome.
    /// Modifies history in place (pushes assistant + tool messages).
    pub async fn drive(&self, history: &mut EntryHistory) -> TurnOutcome {
        let messages = history.messages();
        let tools = self.registry.definitions();

        let chat_response = match self.client.chat(messages, Some(tools)).await {
            Ok(r) => r,
            Err(e) => return TurnOutcome::Failed { error: e },
        };

        let choice = match chat_response.choices.first() {
            Some(c) => c,
            None => {
                return TurnOutcome::Text {
                    text: "(empty response)".into(),
                    entry_id: history.last_id().unwrap_or(0),
                };
            }
        };

        let model_done = choice.finish_reason == "stop";
        let text = choice.message.content_text().to_owned();

        let parent_id = history.last_id().unwrap_or(0);
        let asst_entry_id = history.push_assistant(parent_id, choice.message.clone());

        if model_done {
            return TurnOutcome::Text { text, entry_id: asst_entry_id };
        }

        let tool_calls = match choice.message.tool_calls() {
            Some(tc) => tc.clone(),
            None => {
                return TurnOutcome::Text { text, entry_id: asst_entry_id };
            }
        };

        log::info!("Executing {} tool call(s)", tool_calls.len());

        let tool_results = execute_tool_calls(&tool_calls, &self.registry).await;
        let mut background_tasks = Vec::new();

        for (call, outcome) in tool_results {
            let formatted = crate::tool::format_tool_outcome(&outcome);

            if let ToolOutcome::Immediate {
                ref content,
                is_error: true,
            } = outcome
            {
                log::warn!("Tool error: {}", content);
            }

            if let ToolOutcome::Background { task_id, .. } = &outcome {
                background_tasks.push(task_id.clone());
            }

            if let ToolOutcome::Subagent { handle } = &outcome {
                background_tasks.push(handle.task_id.clone());
            }

            // Persist tool result as individual file
            if let Some(ref dir) = self.tool_results_dir {
                let _ = std::fs::create_dir_all(dir);
                let path = dir.join(format!("{}.txt", call.id));
                if let Err(e) = std::fs::write(&path, &formatted) {
                    log::warn!("Failed to write tool result {}: {}", call.id, e);
                }
            }

            let tool_msg = Message::Tool {
                tool_call_id: call.id.clone(),
                name: None,
                content: formatted,
            };
            history.push_tool(asst_entry_id, tool_msg);
        }

        TurnOutcome::Tools {
            text,
            entry_id: asst_entry_id,
            background_tasks,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool execution helper (moved from agent_loop.rs)
// ---------------------------------------------------------------------------

/// Execute tool calls with wave-based concurrency.
/// Tools with no `depends_on` run concurrently in wave 0.
/// Tools depending on wave-0 tools run sequentially in wave 1.
pub async fn execute_tool_calls<'a>(
    tool_calls: &'a [ToolCall],
    registry: &'a ToolRegistry,
) -> Vec<(&'a ToolCall, ToolOutcome)> {
    let (independent, dependent): (Vec<_>, Vec<_>) =
        tool_calls.iter().partition(|tc| tc.depends_on.is_none());

    let mut results = Vec::with_capacity(tool_calls.len());

    // Wave 0: all independent tools concurrently
    if !independent.is_empty() {
        let futures: Vec<_> = independent
            .into_iter()
            .map(|call| {
                let name = call.function.name.clone();
                let args = call.function.arguments.clone();
                async move {
                    log::info!("Tool call: {}({})", name, args);
                    (call, registry.execute(&name, &args).await)
                }
            })
            .collect();
        results.extend(futures::future::join_all(futures).await);
    }

    // Wave 1+: dependent tools sequentially
    for call in &dependent {
        let dep_id = call.depends_on.as_deref().unwrap_or("");
        if !tool_calls.iter().any(|tc| tc.id == dep_id) {
            log::warn!(
                "Tool call {} depends on unknown ID '{}', running anyway",
                call.function.name,
                dep_id
            );
        }
        log::info!(
            "Tool call (dependent): {}({})",
            call.function.name,
            call.function.arguments
        );
        let outcome = registry
            .execute(&call.function.name, &call.function.arguments)
            .await;
        results.push((call, outcome));
    }

    results
}
