use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::entry::EntryHistory;
use crate::agent::runtime::subagent::SubagentResult;
use crate::error::Error;
use crate::provider::moonshot::tool::ToolCall;
use crate::provider::moonshot::{Message, MoonshotClient};
use crate::tool::{BackgroundTaskResult, ToolOutcome, ToolRegistry};
use crate::trajectory::{SharedTrajectoryRecorder, TrajectoryEventDraft};

// ---------------------------------------------------------------------------
// TurnOutcome — result of driving one LLM turn
// ---------------------------------------------------------------------------

pub enum TurnOutcome {
    /// Model generated text response with no tool calls.
    Text {
        text: String,
        entry_id: usize,
        prompt_tokens: usize,
        completion_tokens: usize,
    },

    /// Model generated tool calls (may include text).
    /// background_tasks contains IDs of async tasks to wait for.
    Tools {
        text: String,
        entry_id: usize,
        background_tasks: Vec<String>,
        prompt_tokens: usize,
        completion_tokens: usize,
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
    bg_tx: mpsc::Sender<BackgroundTaskResult>,
    child_agent_tx: mpsc::Sender<SubagentResult>,
    recorder: SharedTrajectoryRecorder,
    session_id: Option<String>,
    agent_id: String,
}

impl TurnDriver {
    pub fn new(
        client: Arc<MoonshotClient>,
        registry: Arc<ToolRegistry>,
        tool_results_dir: Option<PathBuf>,
        bg_tx: mpsc::Sender<BackgroundTaskResult>,
        child_agent_tx: mpsc::Sender<SubagentResult>,
        recorder: SharedTrajectoryRecorder,
        session_id: Option<String>,
        agent_id: String,
    ) -> Self {
        Self {
            client,
            registry,
            tool_results_dir,
            bg_tx,
            child_agent_tx,
            recorder,
            session_id,
            agent_id,
        }
    }

    /// Drive one turn: call LLM, execute tools, return outcome.
    /// Modifies history in place (pushes assistant + tool messages).
    pub async fn drive(&self, history: &mut EntryHistory, turn_id: &str) -> TurnOutcome {
        let messages = history.messages();
        let tools = self.registry.definitions();

        self.record(
            "model.requested",
            Some(turn_id),
            None,
            serde_json::json!({
                "message_count": messages.len(),
                "tool_count": tools.len(),
            }),
        );

        let chat_response = match self.client.chat(messages, Some(tools)).await {
            Ok(r) => r,
            Err(e) => {
                self.record(
                    "model.failed",
                    Some(turn_id),
                    None,
                    serde_json::json!({ "error": e.to_string() }),
                );
                return TurnOutcome::Failed { error: e };
            }
        };

        let prompt_tokens = chat_response.usage.prompt_tokens;
        let completion_tokens = chat_response.usage.completion_tokens;

        let choice = match chat_response.choices.first() {
            Some(c) => c,
            None => {
                self.record(
                    "model.responded",
                    Some(turn_id),
                    None,
                    serde_json::json!({
                        "response_id": chat_response.id.clone(),
                        "choice_count": 0,
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                    }),
                );
                return TurnOutcome::Text {
                    text: "(empty response)".into(),
                    entry_id: history.last_id().unwrap_or(0),
                    prompt_tokens,
                    completion_tokens,
                };
            }
        };

        let model_done = choice.finish_reason == "stop";
        let text = choice.message.content_text().to_owned();

        self.record(
            "model.responded",
            Some(turn_id),
            None,
            serde_json::json!({
                "response_id": chat_response.id.clone(),
                "finish_reason": choice.finish_reason.clone(),
                "choice_count": chat_response.choices.len(),
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": chat_response.usage.total_tokens,
                "cached_tokens": chat_response.usage.cached_tokens,
            }),
        );

        let parent_id = history.last_id().unwrap_or(0);
        let asst_entry_id = history.push_assistant(parent_id, choice.message.clone());

        if model_done {
            return TurnOutcome::Text {
                text,
                entry_id: asst_entry_id,
                prompt_tokens,
                completion_tokens,
            };
        }

        let tool_calls = match choice.message.tool_calls() {
            Some(tc) => tc.clone(),
            None => {
                return TurnOutcome::Text {
                    text,
                    entry_id: asst_entry_id,
                    prompt_tokens,
                    completion_tokens,
                };
            }
        };

        log::info!("Executing {} tool call(s)", tool_calls.len());
        let task_id = format!("task_{}", turn_id.trim_start_matches("turn_"));
        self.record_with_task(
            "task.created",
            Some(turn_id),
            Some(&task_id),
            None,
            serde_json::json!({
                "assistant_entry_id": asst_entry_id,
                "tool_call_count": tool_calls.len(),
                "tool_call_ids": tool_calls.iter().map(|call| call.id.clone()).collect::<Vec<_>>(),
            }),
        );

        let tool_results = execute_tool_calls(
            &tool_calls,
            &self.registry,
            &self.recorder,
            self.session_id.clone(),
            &self.agent_id,
            turn_id,
            &task_id,
        )
        .await;
        let mut background_tasks = Vec::new();
        let mut tool_error_count = 0usize;

        for (call, outcome) in tool_results {
            let formatted = crate::tool::format_tool_outcome(&outcome);

            if let ToolOutcome::Immediate {
                ref content,
                is_error: true,
            } = outcome
            {
                log::warn!("Tool error: {}", content);
                tool_error_count += 1;
            }

            match &outcome {
                ToolOutcome::Background { task_id, .. } => {
                    background_tasks.push(task_id.clone());
                }
                ToolOutcome::Subagent { handle } => {
                    background_tasks.push(handle.task_id.clone());
                    self.record(
                        "subagent.spawned",
                        Some(turn_id),
                        Some(&call.id),
                        serde_json::json!({
                            "child_agent_id": handle.task_id.clone(),
                            "spawned_by_tool_call_id": call.id.clone(),
                        }),
                    );
                }
                _ => {}
            }

            // Spawn forwarding tasks for background/subagent results.
            // We must do this *after* the match above so the borrow of `outcome` ends.
            // But `outcome` is still owned, so we can move it into the match below.
            match outcome {
                ToolOutcome::Background {
                    task_id, receiver, ..
                } => {
                    let bg_tx = self.bg_tx.clone();
                    tokio::spawn(async move {
                        match receiver.await {
                            Ok(result) => {
                                if let Err(e) = bg_tx.send(result).await {
                                    log::warn!("Failed to forward background task result: {}", e);
                                }
                            }
                            Err(_) => {
                                log::warn!("Background task {} receiver dropped", task_id);
                            }
                        }
                    });
                }
                ToolOutcome::Subagent { handle } => {
                    let child_agent_tx = self.child_agent_tx.clone();
                    let task_id = handle.task_id.clone();
                    tokio::spawn(async move {
                        match handle.result_rx.await {
                            Ok(result) => {
                                if let Err(e) = child_agent_tx.send(result).await {
                                    log::warn!("Failed to forward subagent result: {}", e);
                                }
                            }
                            Err(_) => {
                                log::warn!("Subagent {} result receiver dropped", task_id);
                            }
                        }
                    });
                }
                _ => {}
            }

            // Persist tool result as individual file (async)
            if let Some(ref dir) = self.tool_results_dir {
                if let Err(e) = tokio::fs::create_dir_all(dir).await {
                    log::warn!("Failed to create tool results dir: {}", e);
                }
                let path = dir.join(format!("{}.txt", call.id));
                if let Err(e) = tokio::fs::write(&path, &formatted).await {
                    log::warn!("Failed to write tool result {}: {}", call.id, e);
                } else {
                    self.record(
                        "artifact.written",
                        Some(turn_id),
                        Some(&call.id),
                        serde_json::json!({
                            "artifact_kind": "tool_result",
                            "path": path.display().to_string(),
                        }),
                    );
                }
            }

            let tool_msg = Message::Tool {
                tool_call_id: call.id.clone(),
                name: None,
                content: formatted,
            };
            history.push_tool(asst_entry_id, tool_msg);
        }

        self.record_with_task(
            "task.completed",
            Some(turn_id),
            Some(&task_id),
            None,
            serde_json::json!({
                "assistant_entry_id": asst_entry_id,
                "tool_call_count": tool_calls.len(),
                "tool_error_count": tool_error_count,
                "background_task_count": background_tasks.len(),
            }),
        );

        TurnOutcome::Tools {
            text,
            entry_id: asst_entry_id,
            background_tasks,
            prompt_tokens,
            completion_tokens,
        }
    }

    fn record(
        &self,
        event_type: &str,
        turn_id: Option<&str>,
        tool_call_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        self.record_with_task(event_type, turn_id, None, tool_call_id, payload);
    }

    fn record_with_task(
        &self,
        event_type: &str,
        turn_id: Option<&str>,
        task_id: Option<&str>,
        tool_call_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        self.recorder.record(
            TrajectoryEventDraft::new(
                event_type,
                "agent.runtime.turn_driver",
                self.agent_id.clone(),
            )
            .with_session_id(self.session_id.clone())
            .with_turn_id(turn_id.map(str::to_owned))
            .with_task_id(task_id.map(str::to_owned))
            .with_tool_call_id(tool_call_id.map(str::to_owned))
            .with_payload(payload),
        );
    }
}

// ---------------------------------------------------------------------------
// Tool execution helper (moved from agent_loop.rs)
// ---------------------------------------------------------------------------

/// Execute tool calls with wave-based topological execution.
///
/// Tools are grouped into waves where all tools in a wave have their dependencies
/// satisfied by previous waves. Within each wave, tools execute concurrently.
///
/// # Example
/// - Tool A: no dependencies → wave 0
/// - Tool B: depends on A → wave 1  
/// - Tool C: depends on B → wave 2
/// - Tool D: no dependencies → wave 0 (runs concurrently with A)
pub async fn execute_tool_calls<'a>(
    tool_calls: &'a [ToolCall],
    registry: &'a ToolRegistry,
    recorder: &SharedTrajectoryRecorder,
    session_id: Option<String>,
    agent_id: &str,
    turn_id: &str,
    task_id: &str,
) -> Vec<(&'a ToolCall, ToolOutcome)> {
    if tool_calls.is_empty() {
        return Vec::new();
    }

    // Build lookup: tool_id -> ToolCall
    let id_to_call: std::collections::HashMap<&str, &ToolCall> =
        tool_calls.iter().map(|tc| (tc.id.as_str(), tc)).collect();

    // Track which tools are completed
    let mut completed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut pending: Vec<&ToolCall> = tool_calls.iter().collect();
    let mut results = Vec::with_capacity(tool_calls.len());

    while !pending.is_empty() {
        // Find tools whose dependencies are all satisfied.
        // A dependency is satisfied if:
        // 1. It's None (no dependency)
        // 2. The dependency has completed in a previous wave
        // 3. The dependency ID doesn't exist in this batch (lenient: treat as satisfied)
        let (ready, still_pending): (Vec<_>, Vec<_>) =
            pending
                .into_iter()
                .partition(|call| match &call.depends_on {
                    None => true,
                    Some(dep_id) => {
                        completed.contains(dep_id.as_str())
                            || !id_to_call.contains_key(dep_id.as_str())
                    }
                });

        if ready.is_empty() && !still_pending.is_empty() {
            // Cyclic dependency — remaining tools can never run.
            for call in &still_pending {
                let dep_id = call.depends_on.as_deref().unwrap_or("unknown");
                recorder.record(
                    TrajectoryEventDraft::new(
                        "task.dependency.cycle_detected",
                        "agent.runtime.turn_driver",
                        agent_id.to_owned(),
                    )
                    .with_session_id(session_id.clone())
                    .with_turn_id(Some(turn_id.to_owned()))
                    .with_task_id(Some(task_id.to_owned()))
                    .with_tool_call_id(Some(call.id.clone()))
                    .with_payload(serde_json::json!({
                        "name": call.function.name.clone(),
                        "depends_on": dep_id,
                    })),
                );
                log::error!(
                    "Cyclic dependency detected: tool '{}' depends on '{}' which forms a cycle",
                    call.function.name,
                    dep_id
                );
                results.push((
                    *call,
                    ToolOutcome::Immediate {
                        content: format!(
                            "Error: Cannot execute tool '{}' — cyclic dependency on '{}'",
                            call.function.name, dep_id
                        ),
                        is_error: true,
                    },
                ));
            }
            break;
        }

        // Execute ready tools concurrently (one wave)
        if !ready.is_empty() {
            let wave_futures: Vec<_> = ready
                .into_iter()
                .map(|call| {
                    let name = call.function.name.clone();
                    let args = call.function.arguments.clone();
                    let id = call.id.clone();
                    let depends_on = call.depends_on.clone();
                    let recorder = recorder.clone();
                    let session_id = session_id.clone();
                    let agent_id = agent_id.to_owned();
                    let turn_id = turn_id.to_owned();
                    let task_id = task_id.to_owned();
                    async move {
                        log::info!("Tool call: {}({})", name, args);
                        recorder.record(
                            TrajectoryEventDraft::new(
                                "tool.call.started",
                                "agent.runtime.turn_driver",
                                agent_id.clone(),
                            )
                            .with_session_id(session_id.clone())
                            .with_turn_id(Some(turn_id.clone()))
                            .with_task_id(Some(task_id.clone()))
                            .with_tool_call_id(Some(id.clone()))
                            .with_actor(format!("tool:{}", name))
                            .with_payload(serde_json::json!({
                                "name": name.clone(),
                                "arguments_len": args.len(),
                                "depends_on": depends_on,
                            })),
                        );
                        let outcome = registry.execute(&name, &args).await;
                        let (event_type, outcome_kind, is_error) = match &outcome {
                            ToolOutcome::Immediate { is_error, .. } => (
                                if *is_error {
                                    "tool.call.failed"
                                } else {
                                    "tool.call.completed"
                                },
                                "immediate",
                                *is_error,
                            ),
                            ToolOutcome::Background { .. } => {
                                ("tool.call.completed", "background", false)
                            }
                            ToolOutcome::Subagent { .. } => {
                                ("tool.call.completed", "subagent", false)
                            }
                        };
                        recorder.record(
                            TrajectoryEventDraft::new(
                                event_type,
                                "agent.runtime.turn_driver",
                                agent_id,
                            )
                            .with_session_id(session_id)
                            .with_turn_id(Some(turn_id))
                            .with_task_id(Some(task_id))
                            .with_tool_call_id(Some(id.clone()))
                            .with_actor(format!("tool:{}", name))
                            .with_payload(serde_json::json!({
                                "name": name.clone(),
                                "outcome_kind": outcome_kind,
                                "is_error": is_error,
                            })),
                        );
                        log::info!("Tool call {} completed", id);
                        (call, outcome)
                    }
                })
                .collect();

            let wave_results = futures::future::join_all(wave_futures).await;
            for (call, outcome) in wave_results {
                completed.insert(call.id.as_str());
                results.push((call, outcome));
            }
        }

        pending = still_pending;
    }

    results
}
