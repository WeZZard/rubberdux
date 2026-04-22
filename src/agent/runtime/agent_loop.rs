use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::entry::EntryHistory;
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::{BackgroundTaskResult, ToolRegistry};

use super::compaction::CompactionStrategy;
use super::port::{EntryNotification, InputPort, InternalMutation, LoopEvent, LoopOutput, OutputPort};
use super::session::{append_entry_to_session, load_session};
use super::subagent::{ContextEvent, SubagentResult};

// ---------------------------------------------------------------------------
// Private types
// ---------------------------------------------------------------------------

/// Result of a single LLM tool turn.
enum TurnResult {
    /// Model stopped (finish_reason="stop"). Send response to user.
    Done { text: String, entry_id: usize },
    /// Sync tools executed, need another LLM call immediately.
    Continue,
    /// Background tasks dispatched. Wait for all to complete.
    Pending {
        asst_entry_id: usize,
        task_ids: Vec<String>,
    },
}

/// Runtime state for a group of background/subagent tasks.
struct TaskGroup {
    reply: Option<mpsc::Sender<LoopOutput>>,
    asst_entry_id: usize,
    remaining: usize,
    completed_results: Vec<BackgroundTaskResult>,
    metadata: Option<Box<dyn std::any::Any + Send>>,
}

/// Active conversation state. Persists across select! iterations.
struct ActiveConversation {
    reply: Option<mpsc::Sender<LoopOutput>>,
    /// Background task IDs accumulated across multiple tool turns.
    pending_task_ids: Vec<String>,
    /// The assistant entry that first dispatched background tasks (for TaskGroup).
    first_bg_asst_entry_id: Option<usize>,
    /// Opaque metadata from the input source (e.g. telegram_message_id).
    metadata: Option<Box<dyn std::any::Any + Send>>,
    /// The Telegram message ID this conversation is replying to.
    /// Stored separately so it can be cloned for intermediate responses.
    reply_to_message_id: Option<i32>,
    /// Whether this conversation was triggered by a task completion.
    is_child_completion: bool,
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for constructing an AgentLoop.
pub struct AgentLoopConfig {
    pub client: Arc<MoonshotClient>,
    pub registry: Arc<ToolRegistry>,
    pub system_prompt: String,
    pub session_path: Option<PathBuf>,
    /// Directory for persisting individual tool call results.
    pub tool_results_dir: Option<PathBuf>,
    pub token_budget: usize,
    pub cancel: CancellationToken,
    pub compaction: Box<dyn CompactionStrategy>,
    /// Optional pre-created context broadcast sender. If None, a new one is created.
    /// Use this when a tool (e.g. AgentTool) needs the sender before the loop is constructed.
    pub context_tx: Option<broadcast::Sender<ContextEvent>>,
}

/// The unified agent loop that drives LLM conversations, tool execution,
/// background task tracking, and subagent coordination.
pub struct AgentLoop {
    // From config
    client: Arc<MoonshotClient>,
    registry: Arc<ToolRegistry>,
    system_prompt: String,
    session_path: Option<PathBuf>,
    tool_results_dir: Option<PathBuf>,
    token_budget: usize,
    cancel: CancellationToken,
    compaction: Box<dyn CompactionStrategy>,

    // History
    history: EntryHistory,
    current_tokens: usize,

    // Channels
    input_rx: mpsc::Receiver<LoopEvent>,
    trampoline_tx: mpsc::Sender<()>,
    trampoline_rx: mpsc::Receiver<()>,
    bg_tx: mpsc::Sender<BackgroundTaskResult>,
    bg_rx: mpsc::Receiver<BackgroundTaskResult>,
    child_agent_tx: mpsc::Sender<SubagentResult>,
    child_agent_rx: mpsc::Receiver<SubagentResult>,
    context_tx: broadcast::Sender<ContextEvent>,
    entry_notify_tx: broadcast::Sender<EntryNotification>,

    // State
    active_groups: HashMap<usize, TaskGroup>,
    task_to_group: HashMap<String, usize>,
    active_conversation: Option<ActiveConversation>,
    active_child_cancels: Vec<CancellationToken>,
    /// Messages that arrived while a turn was in progress.
    pending_messages: Vec<(Message, Option<mpsc::Sender<LoopOutput>>, Option<Box<dyn std::any::Any + Send>>)>,
}

impl AgentLoop {
    /// Create a new AgentLoop and its InputPort for injecting events.
    pub fn new(config: AgentLoopConfig) -> (Self, InputPort) {
        let (input_tx, input_rx) = mpsc::channel(32);

        let history = match &config.session_path {
            Some(path) => load_session(path, &config.system_prompt),
            None => {
                let mut h = EntryHistory::new();
                h.push_system(Message::System {
                    content: config.system_prompt.clone(),
                });
                h
            }
        };

        let (trampoline_tx, trampoline_rx) = mpsc::channel(16);
        let (bg_tx, bg_rx) = mpsc::channel(32);
        let (child_agent_tx, child_agent_rx) = mpsc::channel(32);
        let context_tx = config.context_tx.unwrap_or_else(|| broadcast::channel(64).0);
        let (entry_notify_tx, _) = broadcast::channel(256);

        let agent_loop = Self {
            client: config.client,
            registry: config.registry,
            system_prompt: config.system_prompt,
            session_path: config.session_path,
            tool_results_dir: config.tool_results_dir,
            token_budget: config.token_budget,
            cancel: config.cancel,
            compaction: config.compaction,
            history,
            current_tokens: 0,
            input_rx,
            trampoline_tx,
            trampoline_rx,
            bg_tx,
            bg_rx,
            child_agent_tx,
            child_agent_rx,
            context_tx,
            entry_notify_tx,
            active_groups: HashMap::new(),
            task_to_group: HashMap::new(),
            active_conversation: None,
            active_child_cancels: Vec::new(),
            pending_messages: Vec::new(),
        };

        let input_port = InputPort::new(input_tx);
        (agent_loop, input_port)
    }

    /// Subscribe to entry notifications broadcast by this loop.
    pub fn subscribe_output(&self) -> OutputPort {
        OutputPort::new(self.entry_notify_tx.subscribe())
    }

    /// Get the context broadcast sender (for spawning child agents that need context updates).
    pub fn context_sender(&self) -> broadcast::Sender<ContextEvent> {
        self.context_tx.clone()
    }

    // -----------------------------------------------------------------------
    // Core event loop
    // -----------------------------------------------------------------------

    /// Run the agent loop until cancellation or all channels close.
    pub async fn run(mut self) {
        log::info!(
            "AgentLoop started (history: {} messages, session: {:?})",
            self.history.len(),
            self.session_path,
        );

        loop {
            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    log::info!("AgentLoop cancelled");
                    break;
                }

                Some(event) = self.input_rx.recv() => {
                    self.handle_event(event).await;
                }

                Some(bg_result) = self.bg_rx.recv() => {
                    self.handle_task_result(bg_result).await;
                }

                Some(agent_result) = self.child_agent_rx.recv() => {
                    let bg_result = BackgroundTaskResult {
                        task_id: agent_result.task_id,
                        content: agent_result.summary,
                    };
                    self.handle_task_result(bg_result).await;
                }

                Some(()) = self.trampoline_rx.recv(), if self.active_conversation.is_some() => {
                    self.drain_pending_events().await;
                    self.drive_conversation().await;
                }

                else => break,
            }
        }

        log::info!("AgentLoop shutting down");
    }

    /// Run the loop until the model stops with no pending tasks.
    /// Returns the final assistant text. Convenience for subagent use.
    pub async fn run_to_completion(mut self) -> String {
        loop {
            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    return "(cancelled)".into();
                }

                Some(event) = self.input_rx.recv() => {
                    self.handle_event(event).await;
                }

                Some(bg_result) = self.bg_rx.recv() => {
                    self.handle_task_result(bg_result).await;
                }

                Some(agent_result) = self.child_agent_rx.recv() => {
                    let bg_result = BackgroundTaskResult {
                        task_id: agent_result.task_id,
                        content: agent_result.summary,
                    };
                    self.handle_task_result(bg_result).await;
                }

                Some(()) = self.trampoline_rx.recv(), if self.active_conversation.is_some() => {
                    self.drain_pending_events().await;
                    self.drive_conversation().await;
                }

                else => break,
            }

            // Check exit condition after every branch, not just trampoline.
            // Done when: no active conversation AND no pending task groups.
            if self.active_conversation.is_none() && self.active_groups.is_empty() {
                // Only exit if we've actually had at least one conversation
                // (history has more than just the system message).
                if self.history.len() > 2 {
                    break;
                }
            }
        }

        // Extract final assistant text from history
        self.history
            .entries()
            .iter()
            .rev()
            .find_map(|e| match &e.message {
                Message::Assistant {
                    content: Some(text), ..
                } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Event handling
    // -----------------------------------------------------------------------

    async fn handle_event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::UserMessage {
                message,
                reply,
                metadata,
            } => {
                let text_estimate = message.content_text().len();

                let _ = self.context_tx.send(ContextEvent::UserMessage(message.clone()));

                if self.active_conversation.is_some() {
                    // A turn is already in progress; queue this message for later.
                    // Do NOT add to history yet — the LLM should only see this message
                    // when we actually start processing it.
                    log::info!("Queueing message while turn in progress (pending: {})", self.pending_messages.len() + 1);
                    self.pending_messages.push((message, reply, metadata));
                } else {
                    if self.current_tokens > 0 {
                        self.compaction.compact(
                            &mut self.history,
                            self.token_budget,
                            self.current_tokens + text_estimate / 4,
                        );
                    }

                    let entry_id = self.history.push_user(message.clone());
                    self.persist_entry(entry_id);
                    let reply_to = metadata
                        .as_ref()
                        .and_then(|m| m.downcast_ref::<i32>().copied());
                    self.active_conversation = Some(ActiveConversation {
                        reply,
                        pending_task_ids: Vec::new(),
                        first_bg_asst_entry_id: None,
                        metadata,
                        reply_to_message_id: reply_to,
                        is_child_completion: false,
                    });
                    let _ = self.trampoline_tx.send(()).await;
                }
            }
            LoopEvent::ContextUpdate(message) => {
                let entry_id = self.history.push_user(message.clone());
                self.persist_entry(entry_id);
                let _ = self
                    .context_tx
                    .send(ContextEvent::EnvironmentChange(message));
                self.notify_entry(entry_id, false);
            }
            LoopEvent::Internal(mutation) => {
                self.handle_internal_mutation(mutation);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task result handling
    // -----------------------------------------------------------------------

    async fn handle_task_result(&mut self, result: BackgroundTaskResult) {
        let group_key = match self.task_to_group.remove(&result.task_id) {
            Some(k) => k,
            None => {
                log::warn!("Received result for unknown task: {}", result.task_id);
                let msg = Message::User {
                    content: UserContent::Text(format!(
                        "[Subagent {} completed. This is a subagent result — the user has not seen this content. Decide whether and how to present it based on the original request.]\n{}",
                        result.task_id, result.content
                    )),
                };
                let entry_id = self.history.push_user(msg);
                self.persist_entry(entry_id);
                self.notify_entry(entry_id, false);
                return;
            }
        };

        let group = match self.active_groups.get_mut(&group_key) {
            Some(g) => g,
            None => {
                log::warn!("No active group for key {}", group_key);
                return;
            }
        };

        log::info!(
            "Task {} completed ({} bytes), {}/{} in group",
            result.task_id,
            result.content.len(),
            group.completed_results.len() + 1,
            group.remaining + group.completed_results.len(),
        );

        group.completed_results.push(result);
        group.remaining -= 1;

        if group.remaining > 0 {
            return;
        }

        // Group complete
        let group = self.active_groups.remove(&group_key);
        let group = match group {
            Some(g) => g,
            None => return,
        };

        for result in &group.completed_results {
            let msg = Message::User {
                content: UserContent::Text(format!(
                    "[Subagent {} completed. This is a subagent result — the user has not seen this content. Decide whether and how to present it based on the original request.]\n{}",
                    result.task_id, result.content
                )),
            };
            let entry_id = self.history.push_user(msg);
            self.persist_entry(entry_id);
            self.notify_entry(entry_id, false);
        }

        // If there's already an active conversation driving the LLM,
        // don't overwrite it — the results are in history and the next
        // LLM turn will see them. Only create a new conversation if idle.
        if self.active_conversation.is_none() {
            let reply_to = group
                .metadata
                .as_ref()
                .and_then(|m| m.downcast_ref::<i32>().copied());
            self.active_conversation = Some(ActiveConversation {
                reply: group.reply,
                pending_task_ids: Vec::new(),
                first_bg_asst_entry_id: None,
                metadata: group.metadata,
                reply_to_message_id: reply_to,
                is_child_completion: true,
            });
            let _ = self.trampoline_tx.send(()).await;
        }
    }

    // -----------------------------------------------------------------------
    // Conversation driving
    // -----------------------------------------------------------------------

    async fn drive_conversation(&mut self) {
        let turn_result = self.run_tool_turn().await;
        log::info!(
            "drive_conversation: turn_result={:?}, active_conversation={:?}, pending_messages={}",
            turn_result.as_ref().map(|_| "Ok"),
            self.active_conversation.as_ref().map(|c| c.reply_to_message_id),
            self.pending_messages.len()
        );

        match turn_result {
            Ok(TurnResult::Done { text, entry_id }) => {
                let mut conv = match self.active_conversation.take() {
                    Some(c) => c,
                    None => return,
                };

                if conv.pending_task_ids.is_empty() {
                    let is_final = self.active_groups.is_empty();
                    let metadata: Option<Box<dyn std::any::Any + Send>> = conv
                        .reply_to_message_id
                        .map(|id| Box::new(id) as _);
                    Self::send_output(&conv.reply, text, entry_id, is_final, metadata)
                        .await;
                } else {
                    // Distribute the reply channel to ALL active groups
                    // so every group completion can deliver results to
                    // the channel. Metadata (e.g. telegram_message_id)
                    // goes to the first group only.
                    let first_asst = conv.first_bg_asst_entry_id.unwrap_or(entry_id);
                    for (&key, group) in self.active_groups.iter_mut() {
                        if group.reply.is_none() {
                            group.reply = conv.reply.clone();
                        }
                        if key == first_asst && group.metadata.is_none() {
                            group.metadata = conv.metadata.take();
                        }
                    }
                    // Send non-final response (results still pending).
                    // Preserve reply_to_message_id so the host can route back
                    // to the correct Telegram message.
                    let metadata: Option<Box<dyn std::any::Any + Send>> = conv
                        .reply_to_message_id
                        .map(|id| Box::new(id) as _);
                    Self::send_output(&conv.reply, text, entry_id, false, metadata).await;
                }
                log::info!("LLM conversation completed");
                // If there are pending messages, start the next turn.
                if !self.pending_messages.is_empty() {
                    let (message, reply, metadata) = self.pending_messages.remove(0);
                    let text_estimate = message.content_text().len();
                    log::info!(
                        "Processing pending message: text_len={}, reply_to={:?}, history_size={}",
                        text_estimate,
                        metadata.as_ref().and_then(|m| m.downcast_ref::<i32>().copied()),
                        self.history.len()
                    );
                    if self.current_tokens > 0 {
                        self.compaction.compact(
                            &mut self.history,
                            self.token_budget,
                            self.current_tokens + text_estimate / 4,
                        );
                    }
                    let entry_id = self.history.push_user(message);
                    self.persist_entry(entry_id);
                    self.notify_entry(entry_id, false);
                    let reply_to = metadata
                        .as_ref()
                        .and_then(|m| m.downcast_ref::<i32>().copied());
                    self.active_conversation = Some(ActiveConversation {
                        reply,
                        pending_task_ids: Vec::new(),
                        first_bg_asst_entry_id: None,
                        metadata,
                        reply_to_message_id: reply_to,
                        is_child_completion: false,
                    });
                    let _ = self.trampoline_tx.send(()).await;
                    log::info!("Started next turn from pending queue (remaining: {})", self.pending_messages.len());
                }
            }
            Ok(TurnResult::Continue) => {
                let _ = self.trampoline_tx.send(()).await;
            }
            Ok(TurnResult::Pending {
                asst_entry_id,
                task_ids,
            }) => {
                if let Some(conv) = self.active_conversation.as_mut() {
                    if conv.first_bg_asst_entry_id.is_none() {
                        conv.first_bg_asst_entry_id = Some(asst_entry_id);
                    }
                    conv.pending_task_ids.extend(task_ids.clone());
                }

                // Register tasks immediately so results arriving before
                // Done can find their group (prevents orphaned results).
                for tid in &task_ids {
                    self.task_to_group.insert(tid.clone(), asst_entry_id);
                }
                if !self.active_groups.contains_key(&asst_entry_id) {
                    self.active_groups.insert(
                        asst_entry_id,
                        TaskGroup {
                            reply: None, // Set when Done is received
                            asst_entry_id,
                            remaining: task_ids.len(),
                            completed_results: Vec::new(),
                            metadata: None,
                        },
                    );
                } else if let Some(group) = self.active_groups.get_mut(&asst_entry_id) {
                    group.remaining += task_ids.len();
                }
                let _ = self.trampoline_tx.send(()).await;
            }
            Err(e) => {
                let conv = match self.active_conversation.take() {
                    Some(c) => c,
                    None => return,
                };
                log::error!("LLM call failed: {}", e);
                Self::send_output(
                    &conv.reply,
                    format!("Sorry, I encountered an error: {}", e),
                    self.history.last_id().unwrap_or(0),
                    true,
                    conv.metadata,
                )
                .await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // LLM tool turn
    // -----------------------------------------------------------------------

    async fn run_tool_turn(&mut self) -> Result<TurnResult, crate::error::Error> {
        let messages = self.history.messages();
        let tools = self.registry.definitions();
        let chat_response = self.client.chat(messages, Some(tools)).await?;

        self.current_tokens = chat_response.usage.prompt_tokens;

        log::debug!(
            "Token usage: prompt={}, completion={}, total={}, cached={}",
            chat_response.usage.prompt_tokens,
            chat_response.usage.completion_tokens,
            chat_response.usage.total_tokens,
            chat_response.usage.cached_tokens,
        );

        let choice = match chat_response.choices.first() {
            Some(c) => c,
            None => {
                return Ok(TurnResult::Done {
                    text: "(empty response)".into(),
                    entry_id: self.history.last_id().unwrap_or(0),
                });
            }
        };

        let model_done = choice.finish_reason == "stop";
        let text = choice.message.content_text().to_owned();

        let parent_id = self.history.last_id().unwrap_or(0);
        let asst_entry_id = self.history.push_assistant(parent_id, choice.message.clone());
        if !model_done {
            self.persist_entry(asst_entry_id);
        }
        self.notify_entry(asst_entry_id, model_done);

        if model_done {
            self.persist_entry(asst_entry_id);
            return Ok(TurnResult::Done {
                text,
                entry_id: asst_entry_id,
            });
        }

        let tool_calls = match choice.message.tool_calls() {
            Some(tc) => tc.clone(),
            None => return Ok(TurnResult::Continue),
        };

        log::info!("Executing {} tool call(s)", tool_calls.len());

        let tool_results = execute_tool_calls(&tool_calls, &self.registry).await;

        let mut bg_task_ids = Vec::new();
        for (call, outcome) in tool_results {
            let formatted = crate::tool::format_tool_outcome(&outcome);

            if let crate::tool::ToolOutcome::Immediate {
                ref content,
                is_error: true,
            } = outcome
            {
                log::warn!("Tool error: {}", content);
            }

            match outcome {
                crate::tool::ToolOutcome::Background {
                    task_id, receiver, ..
                } => {
                    let bg_tx_clone = self.bg_tx.clone();
                    let tid = task_id.clone();
                    tokio::spawn(async move {
                        let result = match receiver.await {
                            Ok(result) => result,
                            Err(_) => {
                                log::warn!("Background task {} sender dropped", tid);
                                BackgroundTaskResult {
                                    task_id: tid,
                                    content: "(task failed: sender dropped)".into(),
                                }
                            }
                        };
                        let _ = bg_tx_clone.send(result).await;
                    });
                    bg_task_ids.push(task_id);
                }
                crate::tool::ToolOutcome::Subagent { handle } => {
                    let agent_tx = self.child_agent_tx.clone();
                    let cancel = handle.cancel.clone();
                    self.active_child_cancels.push(cancel);
                    let task_id = handle.task_id.clone();
                    let tid = task_id.clone();
                    tokio::spawn(async move {
                        let result = match handle.result_rx.await {
                            Ok(result) => result,
                            Err(_) => {
                                log::warn!("Subagent {} sender dropped", tid);
                                SubagentResult {
                                    task_id: tid,
                                    summary: "(subagent failed: sender dropped)".into(),
                                }
                            }
                        };
                        let _ = agent_tx.send(result).await;
                    });
                    bg_task_ids.push(task_id);
                }
                _ => {}
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
            let tool_entry_id = self.history.push_tool(asst_entry_id, tool_msg);
            self.persist_entry(tool_entry_id);
            self.notify_entry(tool_entry_id, false);
        }

        if bg_task_ids.is_empty() {
            Ok(TurnResult::Continue)
        } else {
            Ok(TurnResult::Pending {
                asst_entry_id,
                task_ids: bg_task_ids,
            })
        }
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    fn persist_entry(&self, entry_id: usize) {
        if let (Some(path), Some(entry)) = (&self.session_path, self.history.get(entry_id)) {
            append_entry_to_session(path, entry);
        }
    }

    fn notify_entry(&self, entry_id: usize, is_final: bool) {
        if let Some(entry) = self.history.get(entry_id) {
            let _ = self.entry_notify_tx.send(EntryNotification {
                entry: entry.clone(),
                is_final,
            });
        }
    }

    async fn send_output(
        reply: &Option<mpsc::Sender<LoopOutput>>,
        text: String,
        entry_id: usize,
        is_final: bool,
        metadata: Option<Box<dyn std::any::Any + Send>>,
    ) {
        let reply_to = metadata.as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied());
        log::info!(
            "send_output called: text_len={}, entry_id={}, is_final={}, reply_to={:?}, has_reply={}",
            text.len(),
            entry_id,
            is_final,
            reply_to,
            reply.is_some()
        );
        if let Some(tx) = reply {
            if !text.is_empty() || is_final {
                log::info!("Sending output via reply channel");
                let _ = tx
                    .send(LoopOutput {
                        text,
                        entry_id,
                        is_final,
                        metadata,
                    })
                    .await;
            } else {
                log::info!("Skipping output: text empty and not final");
            }
        } else {
            log::warn!("send_output called but reply channel is None");
        }
    }

    fn handle_internal_mutation(&mut self, mutation: InternalMutation) {
        match mutation {
            InternalMutation::UpdateEntryContent { entry_id, mutator } => {
                if let Some(entry) = self.history.get_mut(entry_id) {
                    mutator(entry);
                }
                self.persist_entry(entry_id);
            }
            InternalMutation::UpdateSystemPrompt { content } => {
                let full = format!("{}\n\n{}", self.system_prompt, content);
                self.history.update_system(full);
                log::info!("Updated system prompt");
            }
        }
    }

    fn finalize_conversation(&mut self, conv: ActiveConversation) {
        if !conv.pending_task_ids.is_empty() {
            let asst_entry_id = conv.first_bg_asst_entry_id.unwrap_or(0);
            for tid in &conv.pending_task_ids {
                self.task_to_group.insert(tid.clone(), asst_entry_id);
            }
            self.active_groups.insert(
                asst_entry_id,
                TaskGroup {
                    reply: conv.reply,
                    asst_entry_id,
                    remaining: conv.pending_task_ids.len(),
                    completed_results: Vec::new(),
                    metadata: conv.metadata,
                },
            );
        }
    }

    async fn drain_pending_events(&mut self) {
        while let Ok(event) = self.input_rx.try_recv() {
            self.handle_event(event).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Module-level helpers
// ---------------------------------------------------------------------------

use crate::provider::moonshot::tool::ToolCall;
use crate::tool::ToolOutcome;

/// Execute tool calls with wave-based concurrency.
/// Tools with no `depends_on` run concurrently in wave 0.
/// Tools depending on wave-0 tools run sequentially in wave 1.
pub(super) async fn execute_tool_calls<'a>(
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
