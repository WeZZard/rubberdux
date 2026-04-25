use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::entry::EntryHistory;
use crate::agent::runtime::history_store::{FilesystemStore, HistoryStore, MemoryStore};
use crate::agent::runtime::port::{
    EntryNotification, InputPort, InternalMutation, LoopEvent, LoopOutput, OutputPort,
};
use crate::agent::runtime::task_coordinator::TaskGroupSet;
use crate::agent::runtime::turn_driver::{TurnDriver, TurnOutcome};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::BackgroundTaskResult;
use crate::trajectory::{
    SharedTrajectoryRecorder, TrajectoryEventDraft, filesystem_recorder, noop_recorder,
};

use super::compaction::CompactionStrategy;
use super::subagent::{ContextEvent, SubagentResult};

// ---------------------------------------------------------------------------
// AgentState — tracks what the agent is currently doing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Waiting for a user message.
    Idle,
    /// HTTP request sent, waiting for response.
    WaitingForResponse,
    /// Processing a response (tool calls, background tasks).
    Processing,
}

// ---------------------------------------------------------------------------
// AgentLoopConfig — configuration for constructing an AgentLoop
// ---------------------------------------------------------------------------

pub struct AgentLoopConfig {
    pub client: Arc<MoonshotClient>,
    pub registry: Arc<crate::tool::ToolRegistry>,
    pub system_prompt: String,
    pub session_path: Option<PathBuf>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub recorder: Option<SharedTrajectoryRecorder>,
    pub tool_results_dir: Option<PathBuf>,
    pub token_budget: usize,
    pub cancel: CancellationToken,
    pub compaction: Box<dyn CompactionStrategy>,
    pub context_tx: Option<broadcast::Sender<ContextEvent>>,
}

// ---------------------------------------------------------------------------
// AgentLoop — event-driven agent that drives LLM conversations
// ---------------------------------------------------------------------------

pub struct AgentLoop {
    // Components
    turn_driver: TurnDriver,
    history_store: Arc<dyn HistoryStore>,

    // State
    history: EntryHistory,
    current_tokens: usize,
    token_budget: usize,
    system_prompt: String,
    session_id: Option<String>,
    agent_id: String,
    recorder: SharedTrajectoryRecorder,
    state: AgentState,
    pending_messages: Vec<(
        Message,
        Option<mpsc::Sender<LoopOutput>>,
        Option<Box<dyn std::any::Any + Send + Sync>>,
    )>,
    next_turn_seq: u64,

    // Channels
    input_rx: mpsc::Receiver<LoopEvent>,
    #[allow(dead_code)]
    bg_tx: mpsc::Sender<BackgroundTaskResult>,
    bg_rx: mpsc::Receiver<BackgroundTaskResult>,
    #[allow(dead_code)]
    child_agent_tx: mpsc::Sender<SubagentResult>,
    child_agent_rx: mpsc::Receiver<SubagentResult>,
    context_tx: broadcast::Sender<ContextEvent>,
    entry_notify_tx: broadcast::Sender<EntryNotification>,

    // Task tracking
    active_groups: TaskGroupSet,

    // Compaction
    compaction: Box<dyn CompactionStrategy>,

    // Cancellation
    cancel: CancellationToken,
}

impl AgentLoop {
    /// Create a new AgentLoop and its InputPort for injecting events.
    pub async fn new(config: AgentLoopConfig) -> (Self, InputPort) {
        let (input_tx, input_rx) = mpsc::channel(32);

        let history_store: Arc<dyn HistoryStore> = match &config.session_path {
            Some(path) => Arc::new(FilesystemStore::new(path.clone())),
            None => Arc::new(MemoryStore::new()),
        };

        // Load history asynchronously via HistoryStore.
        let history = match history_store.load(&config.system_prompt).await {
            Ok(h) => h,
            Err(e) => {
                log::warn!("Failed to load session, starting fresh: {}", e);
                let mut h = EntryHistory::new();
                h.push_system(Message::System {
                    content: config.system_prompt.clone(),
                });
                h
            }
        };

        let (bg_tx, bg_rx) = mpsc::channel::<BackgroundTaskResult>(32);
        let (child_agent_tx, child_agent_rx) = mpsc::channel::<SubagentResult>(32);
        let context_tx = config
            .context_tx
            .unwrap_or_else(|| broadcast::channel(64).0);
        let (entry_notify_tx, _) = broadcast::channel(256);
        let session_id = config.session_id.clone();
        let agent_id = config
            .agent_id
            .clone()
            .unwrap_or_else(|| derive_agent_id(config.session_path.as_ref()));
        let recorder = config.recorder.clone().unwrap_or_else(|| {
            config
                .session_path
                .as_ref()
                .map(|path| filesystem_recorder(path.with_file_name("events.jsonl")))
                .unwrap_or_else(noop_recorder)
        });

        let turn_driver = TurnDriver::new(
            config.client.clone(),
            config.registry.clone(),
            config.tool_results_dir.clone(),
            bg_tx.clone(),
            child_agent_tx.clone(),
            recorder.clone(),
            session_id.clone(),
            agent_id.clone(),
        );

        let agent_loop = Self {
            turn_driver,
            history_store,
            history,
            current_tokens: 0,
            token_budget: config.token_budget,
            system_prompt: config.system_prompt,
            session_id,
            agent_id,
            recorder,
            state: AgentState::Idle,
            pending_messages: Vec::new(),
            next_turn_seq: 0,
            input_rx,
            bg_tx,
            bg_rx,
            child_agent_tx,
            child_agent_rx,
            context_tx,
            entry_notify_tx,
            active_groups: TaskGroupSet::new(),
            compaction: config.compaction,
            cancel: config.cancel,
        };

        agent_loop.record(
            "agent.started",
            None,
            serde_json::json!({
                "history_entries": agent_loop.history.len(),
                "has_session_path": config.session_path.is_some(),
            }),
        );

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
            "AgentLoop started (history: {} messages)",
            self.history.len(),
        );

        loop {
            // If idle and have pending messages, process one immediately
            // without waiting for external events.
            if self.state == AgentState::Idle && !self.pending_messages.is_empty() {
                let (message, reply, metadata) = self.pending_messages.remove(0);
                self.process_message(message, reply, metadata).await;
                continue;
            }

            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    self.record("agent.cancelled", None, serde_json::json!({ "state": format!("{:?}", self.state) }));
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

                else => break,
            }
        }

        self.record(
            "agent.completed",
            None,
            serde_json::json!({ "state": format!("{:?}", self.state) }),
        );
        log::info!("AgentLoop shutting down");
    }

    /// Run the loop until the model stops with no pending tasks.
    /// Returns the final assistant text. Convenience for subagent use.
    pub async fn run_to_completion(mut self) -> String {
        loop {
            // Process pending messages when idle
            if self.state == AgentState::Idle && !self.pending_messages.is_empty() {
                let (message, reply, metadata) = self.pending_messages.remove(0);
                self.process_message(message, reply, metadata).await;
                continue;
            }

            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    self.record("agent.cancelled", None, serde_json::json!({ "state": format!("{:?}", self.state) }));
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

                else => break,
            }

            // Check exit condition: no active processing and no pending task groups.
            if self.state == AgentState::Idle && self.active_groups.is_empty() {
                // Only exit if we've actually had at least one conversation
                // (history has more than just the system message).
                if self.history.len() > 2 {
                    break;
                }
            }
        }

        // Extract final assistant text from history
        let final_text = self
            .history
            .entries()
            .iter()
            .rev()
            .find_map(|e| match &e.message {
                Message::Assistant {
                    content: Some(text),
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();

        self.record(
            "agent.completed",
            None,
            serde_json::json!({
                "state": format!("{:?}", self.state),
                "final_text_len": final_text.len(),
            }),
        );

        final_text
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
                self.handle_user_message(message, reply, metadata).await;
            }
            LoopEvent::ContextUpdate(message) => {
                self.record_context_update(message).await;
            }
            LoopEvent::Internal(mutation) => {
                self.handle_internal_mutation(mutation);
            }
        }
    }

    async fn handle_user_message(
        &mut self,
        message: Message,
        reply: Option<mpsc::Sender<LoopOutput>>,
        metadata: Option<Box<dyn std::any::Any + Send + Sync>>,
    ) {
        let _ = self
            .context_tx
            .send(ContextEvent::UserMessage(message.clone()));

        match self.state {
            AgentState::Idle => {
                // Start a new turn immediately.
                self.process_message(message, reply, metadata).await;
            }
            AgentState::WaitingForResponse | AgentState::Processing => {
                // Queue for later processing.
                log::info!(
                    "Queueing message while {} (pending: {})",
                    match self.state {
                        AgentState::WaitingForResponse => "waiting for response",
                        AgentState::Processing => "processing",
                        _ => unreachable!(),
                    },
                    self.pending_messages.len() + 1
                );
                self.pending_messages.push((message, reply, metadata));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Turn management
    // -----------------------------------------------------------------------

    /// Process a user message: push to history and drive turns.
    async fn process_message(
        &mut self,
        message: Message,
        reply: Option<mpsc::Sender<LoopOutput>>,
        metadata: Option<Box<dyn std::any::Any + Send + Sync>>,
    ) {
        let entry_id = self.history.push_user(message);
        self.persist_entry(entry_id).await;
        self.record_message("message.recorded", entry_id, "user", false);

        let reply_to = metadata
            .as_ref()
            .and_then(|m| m.downcast_ref::<i32>().copied());

        // Drive turns in a loop until we hit a stopping condition.
        self.state = AgentState::WaitingForResponse;
        let current_reply = reply;
        let current_reply_to = reply_to;

        loop {
            // Compact history if token budget exceeded before driving a turn.
            if self.current_tokens > self.token_budget {
                log::info!(
                    "Token budget exceeded: {} > {}, compacting history",
                    self.current_tokens,
                    self.token_budget
                );
                self.compaction
                    .compact(&mut self.history, self.token_budget, self.current_tokens);
                // Re-estimate after compaction (conservative: half the current)
                self.current_tokens = self.current_tokens / 2;
            }

            let turn_id = self.next_turn_id();
            self.record(
                "turn.started",
                Some(&turn_id),
                serde_json::json!({
                    "history_entries": self.history.len(),
                    "parent_entry_id": self.history.last_id(),
                }),
            );

            self.drain_ready_inputs().await;
            let outcome = self.turn_driver.drive(&mut self.history, &turn_id).await;

            // Update token count from the LLM response.
            match &outcome {
                TurnOutcome::Text { prompt_tokens, .. }
                | TurnOutcome::Tools { prompt_tokens, .. } => {
                    self.current_tokens = *prompt_tokens;
                }
                TurnOutcome::Failed { .. } => {}
            }

            let should_continue = self
                .handle_turn_outcome(outcome, &turn_id, &current_reply, current_reply_to)
                .await;

            if !should_continue {
                break;
            }

            // Continue to next turn (all tools were immediate).
            // Reply channels persist across immediate tool turns.
        }
    }

    /// Handle a single turn outcome. Returns true if another turn should be driven
    /// (all tools were immediate), false otherwise.
    async fn handle_turn_outcome(
        &mut self,
        outcome: TurnOutcome,
        turn_id: &str,
        reply: &Option<mpsc::Sender<LoopOutput>>,
        reply_to_message_id: Option<i32>,
    ) -> bool {
        match outcome {
            TurnOutcome::Text {
                text,
                entry_id,
                prompt_tokens,
                completion_tokens,
            } => {
                self.persist_entry(entry_id).await;
                self.record_message("message.recorded", entry_id, "assistant", true);
                self.notify_entry(entry_id, true);

                let is_final = self.active_groups.is_empty();
                let text_len = text.len();
                Self::send_output(reply, text, entry_id, is_final, reply_to_message_id).await;
                self.record_delivery(
                    turn_id,
                    entry_id,
                    is_final,
                    reply,
                    text_len,
                    reply_to_message_id,
                );
                self.record(
                    "turn.completed",
                    Some(turn_id),
                    serde_json::json!({
                        "outcome": "text",
                        "entry_id": entry_id,
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                    }),
                );

                self.state = AgentState::Idle;
                false
            }
            TurnOutcome::Tools {
                text,
                entry_id,
                background_tasks,
                prompt_tokens,
                completion_tokens,
                ..
            } => {
                self.persist_entry(entry_id).await;
                self.record_message("message.recorded", entry_id, "assistant", false);
                self.notify_entry(entry_id, false);

                // Send any text to user immediately.
                if !text.is_empty() {
                    let text_len = text.len();
                    Self::send_output(
                        reply,
                        text,
                        entry_id,
                        false, // not final, tasks pending
                        reply_to_message_id,
                    )
                    .await;
                    self.record_delivery(
                        turn_id,
                        entry_id,
                        false,
                        reply,
                        text_len,
                        reply_to_message_id,
                    );
                }

                if background_tasks.is_empty() {
                    // All tools were immediate; continue to next turn.
                    self.record(
                        "turn.continued",
                        Some(turn_id),
                        serde_json::json!({
                            "reason": "all_tool_calls_completed_immediately",
                            "entry_id": entry_id,
                            "prompt_tokens": prompt_tokens,
                            "completion_tokens": completion_tokens,
                        }),
                    );
                    self.state = AgentState::WaitingForResponse;
                    true
                } else {
                    // Background tasks dispatched.
                    // Preserve reply_to metadata so final responses are replies to the correct message.
                    let metadata = reply_to_message_id
                        .map(|id| Box::new(id) as Box<dyn std::any::Any + Send + Sync>);
                    self.active_groups.register(
                        entry_id,
                        &background_tasks,
                        reply.clone(),
                        metadata,
                    );
                    self.record(
                        "turn.suspended",
                        Some(turn_id),
                        serde_json::json!({
                            "reason": "waiting_for_background_work",
                            "entry_id": entry_id,
                            "background_task_ids": background_tasks,
                            "prompt_tokens": prompt_tokens,
                            "completion_tokens": completion_tokens,
                        }),
                    );
                    self.state = AgentState::Processing;
                    false
                }
            }
            TurnOutcome::Failed { error } => {
                log::error!("LLM call failed: {}", error);
                self.record(
                    "turn.failed",
                    Some(turn_id),
                    serde_json::json!({ "error": error.to_string() }),
                );
                Self::send_output(
                    reply,
                    format!("Sorry, I encountered an error: {}", error),
                    self.history.last_id().unwrap_or(0),
                    true,
                    reply_to_message_id,
                )
                .await;
                self.record_delivery(
                    turn_id,
                    self.history.last_id().unwrap_or(0),
                    true,
                    reply,
                    error.to_string().len(),
                    reply_to_message_id,
                );
                self.state = AgentState::Idle;
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task result handling
    // -----------------------------------------------------------------------

    async fn handle_task_result(&mut self, result: BackgroundTaskResult) {
        // Check if this is an orphaned result (no matching group).
        if let Some(completed) = self.active_groups.complete(result.clone()) {
            // Group completed — inject all results into history.
            for result in &completed.group.completed_results {
                let msg = Message::User {
                    content: UserContent::Text(format!(
                        "[Subagent {} completed. This is a subagent result — the user has not seen this content. Decide whether and how to present it based on the original request.]\n{}",
                        result.task_id, result.content
                    )),
                };
                let entry_id = self.history.push_user(msg);
                self.persist_entry(entry_id).await;
                self.record(
                    "operation.result.injected",
                    None,
                    serde_json::json!({
                        "operation_id": result.task_id.clone(),
                        "entry_id": entry_id,
                    }),
                );
                self.record_message("message.recorded", entry_id, "user", false);
                self.notify_entry(entry_id, false);
            }

            // If no more active groups, start a new turn with the completed results.
            if self.active_groups.is_empty() {
                let reply_to = completed
                    .group
                    .metadata
                    .as_ref()
                    .and_then(|m| m.downcast_ref::<i32>().copied());
                self.state = AgentState::WaitingForResponse;

                // Drive turns with the reply channel from the completed group.
                let current_reply = completed.group.reply;
                let current_reply_to = reply_to;

                loop {
                    let turn_id = self.next_turn_id();
                    self.record(
                        "turn.started",
                        Some(&turn_id),
                        serde_json::json!({
                            "reason": "background_work_completed",
                            "history_entries": self.history.len(),
                            "parent_entry_id": self.history.last_id(),
                        }),
                    );
                    self.drain_ready_inputs().await;
                    let outcome = self.turn_driver.drive(&mut self.history, &turn_id).await;
                    let should_continue = self
                        .handle_turn_outcome(outcome, &turn_id, &current_reply, current_reply_to)
                        .await;

                    if !should_continue {
                        break;
                    }
                }
            }
        } else {
            // Orphaned result — inject directly.
            let msg = Message::User {
                content: UserContent::Text(format!(
                    "[Subagent {} completed. This is a subagent result — the user has not seen this content. Decide whether and how to present it based on the original request.]\n{}",
                    result.task_id, result.content
                )),
            };
            let entry_id = self.history.push_user(msg);
            self.persist_entry(entry_id).await;
            self.record(
                "operation.orphaned",
                None,
                serde_json::json!({
                    "operation_id": result.task_id.clone(),
                    "entry_id": entry_id,
                }),
            );
            self.record_message("message.recorded", entry_id, "user", false);
            self.notify_entry(entry_id, false);
        }
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    async fn persist_entry(&self, entry_id: usize) {
        if let Some(entry) = self.history.get(entry_id) {
            if let Err(e) = self.history_store.persist(entry).await {
                log::error!("Failed to persist entry {}: {}", entry_id, e);
            }
        }
    }

    async fn drain_ready_inputs(&mut self) {
        loop {
            match self.input_rx.try_recv() {
                Ok(LoopEvent::ContextUpdate(message)) => self.record_context_update(message).await,
                Ok(LoopEvent::UserMessage {
                    message,
                    reply,
                    metadata,
                }) => {
                    self.pending_messages.push((message, reply, metadata));
                }
                Ok(LoopEvent::Internal(mutation)) => self.handle_internal_mutation(mutation),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    async fn record_context_update(&mut self, message: Message) {
        let entry_id = self.history.push_user(message.clone());
        self.persist_entry(entry_id).await;
        self.record_message("message.recorded", entry_id, "user", false);
        let _ = self
            .context_tx
            .send(ContextEvent::EnvironmentChange(message));
        self.notify_entry(entry_id, false);
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
        reply_to_message_id: Option<i32>,
    ) {
        log::info!(
            "send_output called: text_len={}, entry_id={}, is_final={}, reply_to={:?}, has_reply={}",
            text.len(),
            entry_id,
            is_final,
            reply_to_message_id,
            reply.is_some()
        );
        if let Some(tx) = reply {
            if !text.is_empty() || is_final {
                log::info!("Sending output via reply channel");
                let metadata = reply_to_message_id
                    .map(|id| Box::new(id) as Box<dyn std::any::Any + Send + Sync>);
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
                // Fire-and-forget async persist
                let store = self.history_store.clone();
                if let Some(entry) = self.history.get(entry_id) {
                    let entry = entry.clone();
                    tokio::spawn(async move {
                        if let Err(e) = store.persist(&entry).await {
                            log::error!("Failed to persist entry {}: {}", entry_id, e);
                        }
                    });
                }
            }
            InternalMutation::UpdateSystemPrompt { content } => {
                let full = format!("{}\n\n{}", self.system_prompt, content);
                self.history.update_system(full);
                log::info!("Updated system prompt");
            }
        }
    }

    fn next_turn_id(&mut self) -> String {
        let seq = self.next_turn_seq;
        self.next_turn_seq += 1;
        format!("turn_{}", seq)
    }

    fn record(&self, event_type: &str, turn_id: Option<&str>, payload: serde_json::Value) {
        self.recorder.record(
            TrajectoryEventDraft::new(
                event_type,
                "agent.runtime.agent_loop",
                self.agent_id.clone(),
            )
            .with_session_id(self.session_id.clone())
            .with_turn_id(turn_id.map(str::to_owned))
            .with_payload(payload),
        );
    }

    fn record_message(&self, event_type: &str, entry_id: usize, role: &str, is_final: bool) {
        let text_len = self
            .history
            .get(entry_id)
            .map(|entry| entry.message.content_text().len())
            .unwrap_or(0);
        self.record(
            event_type,
            None,
            serde_json::json!({
                "entry_id": entry_id,
                "role": role,
                "is_final": is_final,
                "text_len": text_len,
            }),
        );
    }

    fn record_delivery(
        &self,
        turn_id: &str,
        entry_id: usize,
        is_final: bool,
        reply: &Option<mpsc::Sender<LoopOutput>>,
        text_len: usize,
        reply_to_message_id: Option<i32>,
    ) {
        self.record(
            "message.delivered",
            Some(turn_id),
            serde_json::json!({
                "entry_id": entry_id,
                "is_final": is_final,
                "has_reply": reply.is_some(),
                "text_len": text_len,
                "reply_to_message_id": reply_to_message_id,
            }),
        );
    }
}

fn derive_agent_id(session_path: Option<&PathBuf>) -> String {
    session_path
        .and_then(|path| path.parent())
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .and_then(|name| {
            if name == "agent_main" {
                Some("main".to_owned())
            } else {
                name.strip_prefix("agent_").map(str::to_owned)
            }
        })
        .unwrap_or_else(|| "main".to_owned())
}
