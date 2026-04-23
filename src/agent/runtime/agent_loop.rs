use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::entry::EntryHistory;
use crate::agent::runtime::history_store::{FilesystemStore, HistoryStore, MemoryStore};
use crate::agent::runtime::port::{EntryNotification, InputPort, InternalMutation, LoopEvent, LoopOutput, OutputPort};
use crate::agent::runtime::task_coordinator::TaskGroupSet;
use crate::agent::runtime::turn_driver::{TurnDriver, TurnOutcome};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::BackgroundTaskResult;

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
    state: AgentState,
    pending_messages: Vec<(Message, Option<mpsc::Sender<LoopOutput>>, Option<Box<dyn std::any::Any + Send + Sync>>)>,

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
        let context_tx = config.context_tx.unwrap_or_else(|| broadcast::channel(64).0);
        let (entry_notify_tx, _) = broadcast::channel(256);

        let turn_driver = TurnDriver::new(
            config.client.clone(),
            config.registry.clone(),
            config.tool_results_dir.clone(),
            bg_tx.clone(),
            child_agent_tx.clone(),
        );

        let agent_loop = Self {
            turn_driver,
            history_store,
            history,
            current_tokens: 0,
            token_budget: config.token_budget,
            system_prompt: config.system_prompt,
            state: AgentState::Idle,
            pending_messages: Vec::new(),
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

    async fn handle_event(
        &mut self,
        event: LoopEvent,
    ) {
        match event {
            LoopEvent::UserMessage {
                message,
                reply,
                metadata,
            } => {
                self.handle_user_message(message, reply, metadata).await;
            }
            LoopEvent::ContextUpdate(message) => {
                let entry_id = self.history.push_user(message.clone());
                self.persist_entry(entry_id).await;
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

    async fn handle_user_message(
        &mut self,
        message: Message,
        reply: Option<mpsc::Sender<LoopOutput>>,
        metadata: Option<Box<dyn std::any::Any + Send + Sync>>,
    ) {
        let _ = self.context_tx.send(ContextEvent::UserMessage(message.clone()));

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
                self.compaction.compact(
                    &mut self.history,
                    self.token_budget,
                    self.current_tokens,
                );
                // Re-estimate after compaction (conservative: half the current)
                self.current_tokens = self.current_tokens / 2;
            }

            let outcome = self.turn_driver.drive(&mut self.history).await;

            // Update token count from the LLM response.
            match &outcome {
                TurnOutcome::Text { prompt_tokens, .. }
                | TurnOutcome::Tools { prompt_tokens, .. } => {
                    self.current_tokens = *prompt_tokens;
                }
                TurnOutcome::Failed { .. } => {}
            }

            let should_continue = self
                .handle_turn_outcome(
                    outcome,
                    &current_reply,
                    current_reply_to,
                )
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
        reply: &Option<mpsc::Sender<LoopOutput>>,
        reply_to_message_id: Option<i32>,
    ) -> bool {
        match outcome {
            TurnOutcome::Text { text, entry_id, .. } => {
                self.persist_entry(entry_id).await;
                self.notify_entry(entry_id, true);

                let is_final = self.active_groups.is_empty();
                Self::send_output(
                    reply,
                    text,
                    entry_id,
                    is_final,
                    reply_to_message_id,
                )
                .await;

                self.state = AgentState::Idle;
                false
            }
            TurnOutcome::Tools {
                text,
                entry_id,
                background_tasks,
                ..
            } => {
                self.persist_entry(entry_id).await;
                self.notify_entry(entry_id, false);

                // Send any text to user immediately.
                if !text.is_empty() {
                    Self::send_output(
                        reply,
                        text,
                        entry_id,
                        false, // not final, tasks pending
                        reply_to_message_id,
                    )
                    .await;
                }

                if background_tasks.is_empty() {
                    // All tools were immediate; continue to next turn.
                    self.state = AgentState::WaitingForResponse;
                    true
                } else {
                    // Background tasks dispatched.
                    // Preserve reply_to metadata so final responses are replies to the correct message.
                    let metadata = reply_to_message_id.map(|id| {
                        Box::new(id) as Box<dyn std::any::Any + Send + Sync>
                    });
                    self.active_groups.register(
                        entry_id,
                        &background_tasks,
                        reply.clone(),
                        metadata,
                    );
                    self.state = AgentState::Processing;
                    false
                }
            }
            TurnOutcome::Failed { error } => {
                log::error!("LLM call failed: {}", error);
                Self::send_output(
                    reply,
                    format!("Sorry, I encountered an error: {}", error),
                    self.history.last_id().unwrap_or(0),
                    true,
                    reply_to_message_id,
                )
                .await;
                self.state = AgentState::Idle;
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task result handling
    // -----------------------------------------------------------------------

    async fn handle_task_result(
        &mut self,
        result: BackgroundTaskResult,
    ) {
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
                    let outcome = self.turn_driver.drive(&mut self.history).await;
                    let should_continue = self
                        .handle_turn_outcome(
                            outcome,
                            &current_reply,
                            current_reply_to,
                        )
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
            self.notify_entry(entry_id, false);
        }
    }

    // -----------------------------------------------------------------------
    // Small helpers
    // -----------------------------------------------------------------------

    async fn persist_entry(
        &self,
        entry_id: usize,
    ) {
        if let Some(entry) = self.history.get(entry_id) {
            if let Err(e) = self.history_store.persist(entry).await {
                log::error!("Failed to persist entry {}: {}", entry_id, e);
            }
        }
    }

    fn notify_entry(
        &self,
        entry_id: usize,
        is_final: bool,
    ) {
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
                let metadata = reply_to_message_id.map(|id| {
                    Box::new(id) as Box<dyn std::any::Any + Send + Sync>
                });
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

    fn handle_internal_mutation(
        &mut self,
        mutation: InternalMutation,
    ) {
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
}
