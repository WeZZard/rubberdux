use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::channel::{AgentResponse, ChannelEvent, InternalEvent};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::tool::ToolRegistry;

use super::agent_loop::{AgentLoop, AgentLoopConfig};
use super::compaction::EvictOldestTurns;
use super::port::{InternalMutation, LoopEvent, LoopOutput};
use super::session::session_path;

const DEFAULT_BEST_PERFORMANCE_TOKENS: usize = 153_600;

/// Bridge a `LoopOutput` reply channel into the channel-specific `AgentResponse` sender.
fn bridge_reply(original_tx: mpsc::Sender<AgentResponse>) -> mpsc::Sender<LoopOutput> {
    let (loop_tx, mut loop_rx) = mpsc::channel::<LoopOutput>(8);
    tokio::spawn(async move {
        while let Some(output) = loop_rx.recv().await {
            let reply_to = output
                .metadata
                .and_then(|m| m.downcast::<i32>().ok())
                .map(|m| *m);
            let _ = original_tx
                .send(AgentResponse {
                    text: output.text,
                    entry_id: output.entry_id,
                    is_final: output.is_final,
                    reply_to_message_id: reply_to,
                })
                .await;
        }
    });
    loop_tx
}

pub async fn run(
    rx: mpsc::Receiver<ChannelEvent>,
    client: Arc<MoonshotClient>,
    system_prompt: String,
) {
    run_with_session(rx, client, system_prompt, session_path()).await;
}

pub async fn run_with_session(
    mut rx: mpsc::Receiver<ChannelEvent>,
    client: Arc<MoonshotClient>,
    system_prompt: String,
    session_path: std::path::PathBuf,
) {
    let best_perf_tokens: usize = std::env::var("RUBBERDUX_LLM_BEST_PERFORMANCE_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BEST_PERFORMANCE_TOKENS);

    // Create context broadcast early so AgentTool can subscribe
    let (context_tx, _) = tokio::sync::broadcast::channel::<super::subagent::ContextEvent>(64);

    // Derive session directory for subagent sessions and tool results
    let session_dir = session_path.parent().map(|p| p.to_path_buf());
    let tool_results_dir = session_dir.as_ref().map(|d| d.join("tool-results"));

    // Build tool registry
    let registry = {
        use crate::provider::moonshot::tool::bash::MoonshotBashTool;
        use crate::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
        use crate::provider::moonshot::tool::web_search::WebSearchTool;
        use crate::tool::agent::{AgentTool, build_subagent_registries};
        use crate::tool::edit::EditFileTool;
        use crate::tool::glob::GlobTool;
        use crate::tool::grep::GrepTool;
        use crate::tool::read::ReadFileTool;
        use crate::tool::write::WriteFileTool;

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
            system_prompt.clone(),
            context_tx.clone(),
            None,
            None,
        )));
        log::info!(
            "Tool registry: {:?}",
            r.definitions()
                .iter()
                .map(|d| &d.function.name)
                .collect::<Vec<_>>()
        );
        r
    };

    let config = AgentLoopConfig {
        client,
        registry: Arc::new(registry),
        system_prompt,
        session_path: Some(session_path),
        session_id: None,
        agent_id: Some("main".into()),
        recorder: None,
        tool_results_dir,
        token_budget: best_perf_tokens,
        cancel: CancellationToken::new(),
        compaction: Box::new(EvictOldestTurns),
        context_tx: Some(context_tx),
    };

    let (agent_loop, input_port) = AgentLoop::new(config).await;

    // Telegram adapter: convert ChannelEvent -> LoopEvent
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                ChannelEvent::UserInput {
                    interpreted,
                    reply_tx,
                    telegram_message_id,
                } => {
                    let has_attachments = !interpreted.attachments.is_empty();
                    let text = if has_attachments {
                        format!(
                            "{} [with {} attachment(s)]",
                            interpreted.text,
                            interpreted.attachments.len()
                        )
                    } else {
                        interpreted.text.clone()
                    };

                    let message = Message::User {
                        content: UserContent::Text(text),
                    };

                    let reply = reply_tx.map(bridge_reply);

                    let metadata: Option<Box<dyn std::any::Any + Send + Sync>> =
                        telegram_message_id.map(|id| Box::new(id) as _);

                    let event = LoopEvent::UserMessage {
                        message,
                        reply,
                        metadata,
                    };

                    if input_port.send(event).await.is_err() {
                        log::warn!("AgentLoop input channel closed");
                        break;
                    }
                }
                ChannelEvent::ContextUpdate { text } => {
                    let message = Message::User {
                        content: UserContent::Text(text),
                    };

                    let event = LoopEvent::ContextUpdate(message);

                    if input_port.send(event).await.is_err() {
                        log::warn!("AgentLoop input channel closed");
                        break;
                    }
                }
                ChannelEvent::InternalEvent(internal) => {
                    let loop_event = match internal {
                        InternalEvent::UpdateAssistantMessageId {
                            entry_id,
                            message_id,
                        } => LoopEvent::Internal(InternalMutation::UpdateEntryContent {
                            entry_id,
                            mutator: Box::new(move |entry| {
                                if let Message::Assistant {
                                    content: Some(text),
                                    ..
                                } = &mut entry.message
                                {
                                    crate::channel::adapter::telegram::inject_assistant_message_id(
                                        text, message_id,
                                    );
                                }
                            }),
                        }),
                        InternalEvent::UpdateAvailableReactions { reaction_section } => {
                            LoopEvent::Internal(InternalMutation::UpdateSystemPrompt {
                                content: reaction_section,
                            })
                        }
                    };

                    if input_port.send(loop_event).await.is_err() {
                        log::warn!("AgentLoop input channel closed");
                        break;
                    }
                }
            }
        }
    });

    agent_loop.run().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;

    use crate::agent::entry::EntryHistory;
    use crate::provider::moonshot::UserContent;

    #[test]
    fn test_eviction_removes_oldest_pairs() {
        let mut history = EntryHistory::new();
        history.push_system(Message::System {
            content: "sys".into(),
        });
        let u1 = history.push_user(Message::User {
            content: UserContent::Text("old1".into()),
        });
        history.push_assistant(
            u1,
            Message::Assistant {
                content: Some("reply1".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        let u2 = history.push_user(Message::User {
            content: UserContent::Text("old2".into()),
        });
        history.push_assistant(
            u2,
            Message::Assistant {
                content: Some("reply2".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );
        let u3 = history.push_user(Message::User {
            content: UserContent::Text("recent".into()),
        });
        history.push_assistant(
            u3,
            Message::Assistant {
                content: Some("reply3".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
        );

        assert_eq!(history.len(), 7);
        assert!(history.evict_oldest_pair());
        assert_eq!(history.len(), 5);
    }

    #[test]
    fn test_no_eviction_under_threshold() {
        let mut history = EntryHistory::new();
        history.push_system(Message::System {
            content: "sys".into(),
        });
        history.push_user(Message::User {
            content: UserContent::Text("hello".into()),
        });

        let last_prompt_tokens: usize = 100;
        let estimated_new: usize = 10;
        let best_perf_tokens: usize = 153_600;

        assert!(last_prompt_tokens + estimated_new <= best_perf_tokens);
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn test_concurrent_tool_execution() {
        use crate::provider::moonshot::tool::ToolDefinition;
        use crate::tool::{Tool, ToolOutcome, ToolRegistry};
        use std::time::{Duration, Instant};

        struct SlowTool {
            name: &'static str,
        }

        impl Tool for SlowTool {
            fn name(&self) -> &str {
                self.name
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new(self.name, "slow", serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _arguments: &'a str,
            ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ToolOutcome::Immediate {
                        content: "done".into(),
                        is_error: false,
                    }
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(SlowTool { name: "slow_a" }));
        registry.register(Box::new(SlowTool { name: "slow_b" }));

        let start = Instant::now();

        let reg = &registry;
        let tool_futures: Vec<_> = ["slow_a", "slow_b"]
            .iter()
            .map(|name| {
                let name = name.to_string();
                async move { reg.execute(&name, "{}").await }
            })
            .collect();
        let results = futures::future::join_all(tool_futures).await;

        let elapsed = start.elapsed();

        assert_eq!(results.len(), 2);
        for outcome in &results {
            match outcome {
                ToolOutcome::Immediate { content, is_error } => {
                    assert!(!is_error);
                    assert_eq!(content, "done");
                }
                _ => panic!("Expected Immediate outcome"),
            }
        }

        // Concurrent: should take ~100ms, not ~200ms
        assert!(
            elapsed < Duration::from_millis(180),
            "Expected concurrent execution under 180ms, took {:?}",
            elapsed
        );
    }

    fn make_tool_call(
        id: &str,
        name: &str,
        depends_on: Option<&str>,
    ) -> crate::provider::moonshot::tool::ToolCall {
        crate::provider::moonshot::tool::ToolCall {
            index: None,
            id: id.into(),
            r#type: "function".into(),
            function: crate::provider::moonshot::tool::FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
            depends_on: depends_on.map(String::from),
        }
    }

    #[tokio::test]
    async fn test_wave_partitioning_independent() {
        use crate::provider::moonshot::tool::ToolDefinition;
        use crate::tool::{Tool, ToolOutcome, ToolRegistry};
        use std::time::{Duration, Instant};

        struct TimedTool {
            name: &'static str,
        }

        impl Tool for TimedTool {
            fn name(&self) -> &str {
                self.name
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new(self.name, "timed", serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _arguments: &'a str,
            ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    ToolOutcome::Immediate {
                        content: "ok".into(),
                        is_error: false,
                    }
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TimedTool { name: "a" }));
        registry.register(Box::new(TimedTool { name: "b" }));
        registry.register(Box::new(TimedTool { name: "c" }));

        let calls = vec![
            make_tool_call("1", "a", None),
            make_tool_call("2", "b", None),
            make_tool_call("3", "c", None),
        ];

        let start = Instant::now();
        let recorder = crate::trajectory::noop_recorder();
        let results = super::super::turn_driver::execute_tool_calls(
            &calls,
            &registry,
            &recorder,
            None,
            "main",
            "turn_test",
            "task_test",
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        // All 3 independent tools should run concurrently (~80ms, not ~240ms)
        assert!(
            elapsed < Duration::from_millis(160),
            "Expected concurrent execution under 160ms, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_wave_partitioning_dependent() {
        use crate::provider::moonshot::tool::ToolDefinition;
        use crate::tool::{Tool, ToolOutcome, ToolRegistry};
        use std::time::{Duration, Instant};

        struct TimedTool {
            name: &'static str,
        }

        impl Tool for TimedTool {
            fn name(&self) -> &str {
                self.name
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new(self.name, "timed", serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _arguments: &'a str,
            ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    ToolOutcome::Immediate {
                        content: "ok".into(),
                        is_error: false,
                    }
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(TimedTool { name: "a" }));
        registry.register(Box::new(TimedTool { name: "b" }));
        registry.register(Box::new(TimedTool { name: "c" }));

        // A and B independent, C depends on A
        let calls = vec![
            make_tool_call("1", "a", None),
            make_tool_call("2", "b", None),
            make_tool_call("3", "c", Some("1")),
        ];

        let start = Instant::now();
        let recorder = crate::trajectory::noop_recorder();
        let results = super::super::turn_driver::execute_tool_calls(
            &calls,
            &registry,
            &recorder,
            None,
            "main",
            "turn_test",
            "task_test",
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        // Wave 0: A+B concurrent (~80ms), Wave 1: C (~80ms) = ~160ms total
        assert!(
            elapsed >= Duration::from_millis(140),
            "Expected sequential waves to take at least 140ms, took {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(260),
            "Should not take 3x sequential (~240ms+), took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_malformed_depends_on() {
        use crate::provider::moonshot::tool::ToolDefinition;
        use crate::tool::{Tool, ToolOutcome, ToolRegistry};

        struct DummyTool;

        impl Tool for DummyTool {
            fn name(&self) -> &str {
                "d"
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new("d", "dummy", serde_json::json!({}))
            }
            fn execute<'a>(
                &'a self,
                _arguments: &'a str,
            ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
                Box::pin(async {
                    ToolOutcome::Immediate {
                        content: "ok".into(),
                        is_error: false,
                    }
                })
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool));

        // depends_on references a nonexistent ID
        let calls = vec![make_tool_call("1", "d", Some("nonexistent"))];

        let recorder = crate::trajectory::noop_recorder();
        let results = super::super::turn_driver::execute_tool_calls(
            &calls,
            &registry,
            &recorder,
            None,
            "main",
            "turn_test",
            "task_test",
        )
        .await;
        assert_eq!(results.len(), 1);
        match &results[0].1 {
            ToolOutcome::Immediate { content, is_error } => {
                assert!(!is_error);
                assert_eq!(content, "ok");
            }
            _ => panic!("Expected Immediate outcome"),
        }
    }
}
