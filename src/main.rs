mod agent;
#[cfg(feature = "host")]
mod channel;
mod error;
#[cfg(feature = "host")]
mod host;
mod hardened_prompts;
mod protocol;
mod provider;
mod tool;
mod vm;

use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "host")]
use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--agent") {
        let rpc_host = args
            .iter()
            .position(|a| a == "--rpc-host")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "192.168.64.1:19384".to_string());

        let task_id = args
            .iter()
            .position(|a| a == "--task-id")
            .and_then(|i| args.get(i + 1))
            .cloned();

        run_agent(&rpc_host, task_id.as_deref()).await;
    } else {
        #[cfg(feature = "host")]
        {
            // Default: host mode (manage VMs + bridge Telegram)
            run_host().await;
        }
        #[cfg(not(feature = "host"))]
        {
            log::error!("Host mode requires the 'host' feature. Use --agent for agent mode.");
            std::process::exit(1);
        }
    }
}

/// Host mode: thin proxy that manages VMs and bridges Telegram.
#[cfg(feature = "host")]
async fn run_host() {
    log::info!("Starting rubberdux in HOST mode...");

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| {
        log::error!("TELEGRAM_BOT_TOKEN is not set");
        std::process::exit(1);
    });

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(32);
    let (response_tx, mut response_rx) =
        tokio::sync::mpsc::channel::<crate::channel::AgentResponse>(32);

    let bot = Bot::new(bot_token);
    let _bot_for_responses = bot.clone();

    // Spawn response handler: AgentResponse → Telegram
    tokio::spawn(async move {
        while let Some(response) = response_rx.recv().await {
            if response.text.is_empty() {
                continue;
            }
            // Send to the chat. For now, use a hardcoded chat approach.
            // The proper approach would be to track chat_id from the
            // incoming message and route back. This is a placeholder.
            log::info!("Agent response: {}", &response.text[..response.text.len().min(100)]);
        }
    });

    // Run Telegram adapter and host concurrently
    let telegram_task = tokio::spawn(async move {
        channel::adapter::telegram::run(bot, event_tx).await;
    });

    let host_task = tokio::spawn(async move {
        let host_config = host::HostConfig::from_env();
        host::run(host_config, event_rx, response_tx).await;
    });

    tokio::select! {
        _ = telegram_task => log::info!("Telegram adapter stopped"),
        _ = host_task => log::info!("Host stopped"),
    }
}

/// Agent mode: runs inside a VM, connects to host via RPC.
async fn run_agent(rpc_host: &str, task_id: Option<&str>) {
    log::info!(
        "Starting rubberdux in AGENT mode (rpc={}, task={:?})...",
        rpc_host,
        task_id
    );

    let prompt_dir = hardened_prompts::prompt_dir();
    let prompt_parts = hardened_prompts::load_prompt_parts(&prompt_dir);
    let mut system_prompt = hardened_prompts::compose_system_prompt(&prompt_parts, None);

    // If this is a child VM, prepend the subagent-specific preamble if available.
    if task_id.is_some() {
        let share_dir = vm_share_dir();
        let subagent_type_path = format!("{}/subagent_type.txt", share_dir);
        if let Ok(contents) = tokio::fs::read_to_string(&subagent_type_path).await {
            let trimmed = contents.trim();
            if let Ok(subagent_type) =
                serde_json::from_value::<crate::tool::SubagentType>(serde_json::json!(trimmed))
            {
                system_prompt = format!(
                    "{}\n\n{}",
                    hardened_prompts::subagent_preamble(subagent_type),
                    system_prompt
                );
            } else {
                log::warn!("Unrecognized subagent_type in {}: {}", subagent_type_path, trimmed);
            }
        }
    }

    let client = Arc::new(provider::moonshot::MoonshotClient::from_env());

    // Determine persistent data directory (set up by host via symlink)
    let data_dir = std::env::var("RUBBERDUX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/rubberdux-data"));

    let sessions_dir = data_dir.join("sessions");
    let tool_results_dir = data_dir.join("tool-results");
    let subagents_dir = data_dir.join("subagents");

    tokio::fs::create_dir_all(&sessions_dir).await.ok();
    tokio::fs::create_dir_all(&tool_results_dir).await.ok();
    tokio::fs::create_dir_all(&subagents_dir).await.ok();

    let session_file = if let Some(tid) = task_id {
        format!("{}.jsonl", tid)
    } else {
        "session.jsonl".to_string()
    };
    let session_path = sessions_dir.join(session_file);

    // Connect to host RPC with retries to tolerate NAT startup on child VMs.
    let stream = {
        let mut last_err = None;
        let mut stream = None;
        for attempt in 0..30 {
            match tokio::net::TcpStream::connect(rpc_host).await {
                Ok(s) => {
                    log::info!("Connected to host RPC at {} after {} attempts", rpc_host, attempt + 1);
                    stream = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
        match stream {
            Some(s) => s,
            None => {
                log::error!("Failed to connect to host RPC at {}: {}", rpc_host, last_err.unwrap());
                return;
            }
        }
    };

    let (mut rpc_reader, rpc_writer) = stream.into_split();
    let rpc_writer = Arc::new(tokio::sync::Mutex::new(rpc_writer));

    // Build agent loop with tools
    use crate::agent::runtime::agent_loop::{AgentLoop, AgentLoopConfig};
    use crate::agent::runtime::compaction::EvictOldestTurns;
    use crate::agent::runtime::port::LoopOutput;
    use crate::agent::runtime::subagent::ContextEvent;
    use crate::provider::moonshot::{Message, UserContent};
    use crate::tool::ToolRegistry;

    let (context_tx, _) = tokio::sync::broadcast::channel::<ContextEvent>(64);
    let cancel = tokio_util::sync::CancellationToken::new();

    // Build tool registry
    let registry = {
        use crate::provider::moonshot::tool::bash::MoonshotBashTool;
        use crate::provider::moonshot::tool::web_fetch::MoonshotWebFetchTool;
        use crate::provider::moonshot::tool::web_search::WebSearchTool;
        use crate::tool::agent::{build_subagent_registries, AgentTool};
        use crate::tool::edit::EditFileTool;
        use crate::tool::glob::GlobTool;
        use crate::tool::grep::GrepTool;
        use crate::tool::read::ReadFileTool;
        use crate::tool::write::WriteFileTool;

        let last_user_query = Arc::new(std::sync::RwLock::new(String::new()));

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

        // Only the main VM gets the agent tool (and ability to spawn child VMs).
        // Child VMs (task_id is Some) are single-purpose and should not recurse.
        if task_id.is_none() {
            let subagent_registries = build_subagent_registries(&client, &last_user_query);
            r.register(Box::new(AgentTool::new(
                client.clone(),
                subagent_registries,
                system_prompt.clone(),
                context_tx.clone(),
                Some(rpc_writer.clone()),
                Some(subagents_dir),
            )));
        }

        r
    };

    let config = AgentLoopConfig {
        client,
        registry: Arc::new(registry),
        system_prompt,
        session_path: Some(session_path),
        tool_results_dir: Some(tool_results_dir),
        token_budget: 153_600,
        cancel: cancel.clone(),
        compaction: Box::new(EvictOldestTurns),
        context_tx: Some(context_tx),
    };

    let (agent_loop, input_port) = AgentLoop::new(config);

    // Spawn RPC reader: HostToAgent → AgentLoop
    let writer_for_rpc = rpc_writer.clone();
    let input_for_rpc = input_port.clone();
    tokio::spawn(async move {
        loop {
            let msg: Option<protocol::HostToAgent> =
                match protocol::read_message(&mut rpc_reader).await {
                    Ok(m) => m,
                    Err(e) => {
                        log::error!("RPC read error: {}", e);
                        break;
                    }
                };

            match msg {
                Some(protocol::HostToAgent::UserMessage {
                    text,
                    telegram_message_id,
                }) => {
                    log::info!("Agent received UserMessage from host: msg_id={:?}, text_len={}", telegram_message_id, text.len());
                    let message = Message::User {
                        content: UserContent::Text(text),
                    };

                    // Create reply channel that forwards to RPC
                    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<LoopOutput>(8);
                    let w = writer_for_rpc.clone();
                    tokio::spawn(async move {
                        log::info!("Reply channel task started for msg_id={:?}", telegram_message_id);
                        while let Some(output) = reply_rx.recv().await {
                            log::info!("Agent received output from reply channel: entry_id={}, is_final={}, text_len={}", output.entry_id, output.is_final, output.text.len());
                            let msg = protocol::AgentToHost::Response {
                                text: output.text,
                                entry_id: output.entry_id,
                                is_final: output.is_final,
                                reply_to_message_id: output
                                    .metadata
                                    .and_then(|m| m.downcast::<i32>().ok())
                                    .map(|m| *m),
                            };
                            let mut writer = w.lock().await;
                            match protocol::write_message(&mut writer, &msg).await {
                                Ok(_) => log::info!("Response sent to host successfully"),
                                Err(e) => {
                                    log::error!("Failed to send response to host: {}", e);
                                    break;
                                }
                            }
                        }
                    });

                    let metadata: Option<Box<dyn std::any::Any + Send>> =
                        telegram_message_id.map(|id| Box::new(id) as _);

                    use crate::agent::runtime::port::LoopEvent;
                    let event = LoopEvent::UserMessage {
                        message,
                        reply: Some(reply_tx),
                        metadata,
                    };
                    if input_for_rpc.send(event).await.is_err() {
                        log::warn!("AgentLoop input closed");
                        break;
                    }
                }
                Some(protocol::HostToAgent::VMCompleted { task_id, result }) => {
                    // Inject as a user message with a reply channel so the AgentLoop
                    // makes a new LLM call and sends the response back to the host.
                    let message = Message::User {
                        content: UserContent::Text(format!(
                            "[VM agent {} completed. This is a VM subagent result — the user has not seen this content. Decide whether and how to present it based on the original request.]\n{}",
                            task_id, result
                        )),
                    };
                    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<LoopOutput>(8);
                    let w = writer_for_rpc.clone();
                    tokio::spawn(async move {
                        while let Some(output) = reply_rx.recv().await {
                            let msg = protocol::AgentToHost::Response {
                                text: output.text,
                                entry_id: output.entry_id,
                                is_final: output.is_final,
                                reply_to_message_id: None,
                            };
                            let mut writer = w.lock().await;
                            if let Err(e) = protocol::write_message(&mut writer, &msg).await {
                                log::error!("Failed to send VM follow-up to host: {}", e);
                                break;
                            }
                        }
                    });
                    use crate::agent::runtime::port::LoopEvent;
                    let event = LoopEvent::UserMessage {
                        message,
                        reply: Some(reply_tx),
                        metadata: None,
                    };
                    if input_for_rpc.send(event).await.is_err() {
                        break;
                    }
                }
                Some(protocol::HostToAgent::VMFailed { task_id, error }) => {
                    let message = Message::User {
                        content: UserContent::Text(format!(
                            "[VM agent {} failed: {}]",
                            task_id, error
                        )),
                    };
                    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<LoopOutput>(8);
                    let w = writer_for_rpc.clone();
                    tokio::spawn(async move {
                        while let Some(output) = reply_rx.recv().await {
                            let msg = protocol::AgentToHost::Response {
                                text: output.text,
                                entry_id: output.entry_id,
                                is_final: output.is_final,
                                reply_to_message_id: None,
                            };
                            let mut writer = w.lock().await;
                            if let Err(e) = protocol::write_message(&mut writer, &msg).await {
                                log::error!("Failed to send VM failure follow-up to host: {}", e);
                                break;
                            }
                        }
                    });
                    use crate::agent::runtime::port::LoopEvent;
                    let event = LoopEvent::UserMessage {
                        message,
                        reply: Some(reply_tx),
                        metadata: None,
                    };
                    if input_for_rpc.send(event).await.is_err() {
                        break;
                    }
                }
                Some(protocol::HostToAgent::Shutdown) => {
                    log::info!("Host requested shutdown");
                    cancel.cancel();
                    break;
                }
                None => {
                    log::info!("Host disconnected");
                    cancel.cancel();
                    break;
                }
            }
        }
    });

    // If this is a task agent (child VM), read prompt from share and auto-run
    if let Some(tid) = task_id {
        // Read prompt from shared directory
        let prompt_path = format!("{}/prompt.txt", vm_share_dir());
        match tokio::fs::read_to_string(&prompt_path).await {
            Ok(prompt) => {
                let message = Message::User {
                    content: UserContent::Text(prompt),
                };
                let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<LoopOutput>(8);
                let w = rpc_writer.clone();
                tokio::spawn(async move {
                    while let Some(output) = reply_rx.recv().await {
                        let msg = protocol::AgentToHost::Response {
                            text: output.text,
                            entry_id: output.entry_id,
                            is_final: output.is_final,
                            reply_to_message_id: None,
                        };
                        let mut writer = w.lock().await;
                        let _ = protocol::write_message(&mut writer, &msg).await;
                    }
                });

                use crate::agent::runtime::port::LoopEvent;
                let event = LoopEvent::UserMessage {
                    message,
                    reply: Some(reply_tx),
                    metadata: None,
                };
                let _ = input_port.send(event).await;
            }
            Err(e) => {
                log::error!("Failed to read prompt for task {}: {}", tid, e);
                return;
            }
        }

        // Run to completion for task agents
        agent_loop.run_to_completion().await;
    } else {
        // Main agent: run indefinitely
        agent_loop.run().await;
    }
}

/// Returns the Tart shared-directory path inside the VM.
#[cfg(target_os = "macos")]
fn vm_share_dir() -> &'static str {
    "/Volumes/My Shared Files/share"
}

#[cfg(target_os = "linux")]
fn vm_share_dir() -> &'static str {
    "/mnt/shared/share"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn vm_share_dir() -> &'static str {
    "/mnt/shared/share"
}


