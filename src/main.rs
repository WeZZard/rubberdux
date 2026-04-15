mod agent;
mod channel;
mod error;
mod host;
mod hardened_prompts;
mod provider;
mod tool;
mod vm;

use std::sync::Arc;

use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "setup") {
        if args.iter().any(|a| a == "--check") {
            let checks = vm::setup::check_prerequisites().await;
            vm::setup::print_prerequisites(&checks);
        } else {
            // Get optional image name: `rubberdux setup macos15` or `rubberdux setup` (all)
            let image_name = args
                .iter()
                .position(|a| a == "setup")
                .and_then(|i| args.get(i + 1))
                .filter(|s| !s.starts_with('-'))
                .map(|s| s.as_str());

            if let Err(e) = vm::setup::run_setup(image_name).await {
                log::error!("Setup failed: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if args.iter().any(|a| a == "--host") {
        run_host().await;
    } else if args.iter().any(|a| a == "--agent") {
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
        // Legacy mode: run the original in-process Telegram + AgentLoop
        run_legacy().await;
    }
}

/// Host mode: thin proxy that manages VMs and bridges Telegram.
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
        host::run(event_rx, response_tx).await;
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
    let system_prompt = hardened_prompts::compose_system_prompt(&prompt_parts, None);

    let client = Arc::new(provider::moonshot::MoonshotClient::from_env());

    // Connect to host RPC
    let stream = match tokio::net::TcpStream::connect(rpc_host).await {
        Ok(s) => {
            log::info!("Connected to host RPC at {}", rpc_host);
            s
        }
        Err(e) => {
            log::error!("Failed to connect to host RPC at {}: {}", rpc_host, e);
            return;
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

        let subagent_registries = build_subagent_registries(&client, &last_user_query);
        r.register(Box::new(AgentTool::new(
            client.clone(),
            subagent_registries,
            system_prompt.clone(),
            context_tx.clone(),
            Some(rpc_writer.clone()),
            None,
        )));

        r
    };

    let config = AgentLoopConfig {
        client,
        registry: Arc::new(registry),
        system_prompt,
        session_path: None,
        tool_results_dir: None,
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
            let msg: Option<vm::rpc::HostToAgent> =
                match vm::rpc::read_message(&mut rpc_reader).await {
                    Ok(m) => m,
                    Err(e) => {
                        log::error!("RPC read error: {}", e);
                        break;
                    }
                };

            match msg {
                Some(vm::rpc::HostToAgent::UserMessage {
                    text,
                    telegram_message_id,
                }) => {
                    let message = Message::User {
                        content: UserContent::Text(text),
                    };

                    // Create reply channel that forwards to RPC
                    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<LoopOutput>(8);
                    let w = writer_for_rpc.clone();
                    tokio::spawn(async move {
                        while let Some(output) = reply_rx.recv().await {
                            let msg = vm::rpc::AgentToHost::Response {
                                text: output.text,
                                entry_id: output.entry_id,
                                is_final: output.is_final,
                                reply_to_message_id: output
                                    .metadata
                                    .and_then(|m| m.downcast::<i32>().ok())
                                    .map(|m| *m),
                            };
                            let mut writer = w.lock().await;
                            if let Err(e) = vm::rpc::write_message(&mut writer, &msg).await {
                                log::error!("Failed to send response to host: {}", e);
                                break;
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
                Some(vm::rpc::HostToAgent::VMCompleted { task_id, result }) => {
                    // Inject as a user message so the AgentLoop sees the result
                    let message = Message::User {
                        content: UserContent::Text(format!(
                            "[VM agent {} completed. This is a VM subagent result — the user has not seen this content. Decide whether and how to present it based on the original request.]\n{}",
                            task_id, result
                        )),
                    };
                    use crate::agent::runtime::port::LoopEvent;
                    let event = LoopEvent::ContextUpdate(message);
                    if input_for_rpc.send(event).await.is_err() {
                        break;
                    }
                }
                Some(vm::rpc::HostToAgent::VMFailed { task_id, error }) => {
                    let message = Message::User {
                        content: UserContent::Text(format!(
                            "[VM agent {} failed: {}]",
                            task_id, error
                        )),
                    };
                    use crate::agent::runtime::port::LoopEvent;
                    let event = LoopEvent::ContextUpdate(message);
                    if input_for_rpc.send(event).await.is_err() {
                        break;
                    }
                }
                Some(vm::rpc::HostToAgent::Shutdown) => {
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
        let prompt_path = "/Volumes/My Shared Files/share/prompt.txt";
        match tokio::fs::read_to_string(prompt_path).await {
            Ok(prompt) => {
                let message = Message::User {
                    content: UserContent::Text(prompt),
                };
                let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<LoopOutput>(8);
                let w = rpc_writer.clone();
                tokio::spawn(async move {
                    while let Some(output) = reply_rx.recv().await {
                        let msg = vm::rpc::AgentToHost::Response {
                            text: output.text,
                            entry_id: output.entry_id,
                            is_final: output.is_final,
                            reply_to_message_id: None,
                        };
                        let mut writer = w.lock().await;
                        let _ = vm::rpc::write_message(&mut writer, &msg).await;
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

/// Legacy mode: original in-process Telegram + AgentLoop.
async fn run_legacy() {
    log::info!("Starting rubberdux (legacy mode)...");

    let prompt_dir = hardened_prompts::prompt_dir();
    let prompt_parts = hardened_prompts::load_prompt_parts(&prompt_dir);

    let channel_partial = channel::adapter::telegram::channel_prompt();
    let system_prompt = hardened_prompts::compose_system_prompt(&prompt_parts, Some(channel_partial));
    log::info!("Composed system prompt ({} chars)", system_prompt.len());

    let client = Arc::new(provider::moonshot::MoonshotClient::from_env());
    log::info!("Moonshot client initialized (model: {})", client.model());

    let (tx, rx) = tokio::sync::mpsc::channel(32);

    tokio::spawn(agent::runtime::chat::run(rx, client, system_prompt));

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| {
        log::error!("TELEGRAM_BOT_TOKEN is not set");
        std::process::exit(1);
    });
    let bot = Bot::new(bot_token);
    channel::adapter::telegram::run(bot, tx).await;
}
