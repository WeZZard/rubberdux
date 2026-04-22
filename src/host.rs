use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

use crate::channel::{AgentResponse, ChannelEvent};
use crate::error::Error;
use crate::vm::manager::VMManager;
use crate::protocol::{self, AgentToHost, HostToAgent};

const DEFAULT_RPC_PORT: u16 = 19384;

/// Configuration for host mode.
#[derive(Clone)]
pub struct HostConfig {
    pub vm_image: String,
    pub share_root: PathBuf,
    pub rpc_port: u16,
    pub host_ip: String,
    pub agent_binary_path: Option<String>,
    pub agent_env: HashMap<String, String>,
    pub agent_data_dir: Option<PathBuf>,
    pub memory_mb: Option<usize>,
    pub cpu_count: Option<usize>,
}

impl HostConfig {
    pub fn from_env() -> Self {
        let image = std::env::var("RUBBERDUX_VM_IMAGE")
            .ok()
            .map(|raw| {
                crate::vm::setup::get_image(&raw)
                    .map(|img| img.base_vm_name.to_string())
                    .unwrap_or(raw)
            })
            .unwrap_or_else(|| "rubberdux-base-ubuntu24-release".to_string());

        let share_root = std::env::var("RUBBERDUX_VM_SHARES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./vm-shares"));

        let rpc_port: u16 = std::env::var("RUBBERDUX_RPC_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RPC_PORT);

        let host_ip = std::env::var("RUBBERDUX_HOST_IP")
            .unwrap_or_else(|_| "192.168.64.1".to_string());

        let agent_data_dir = std::env::var("RUBBERDUX_AGENT_DATA_DIR")
            .map(PathBuf::from)
            .ok();

        // Propagate LLM configuration to the agent VM
        let mut agent_env = HashMap::new();
        for key in [
            "RUBBERDUX_LLM_BASE_URL",
            "RUBBERDUX_LLM_API_KEY",
            "RUBBERDUX_LLM_MODEL",
            "RUBBERDUX_LLM_USER_AGENT",
        ] {
            if let Ok(value) = std::env::var(key) {
                agent_env.insert(key.to_string(), value);
            }
        }

        Self {
            vm_image: image,
            share_root,
            rpc_port,
            host_ip,
            agent_binary_path: None,
            agent_env,
            agent_data_dir,
            memory_mb: None,
            cpu_count: None,
        }
    }
}

fn build_agent_command(config: &HostConfig, task_id: Option<&str>) -> String {
    let binary = config.agent_binary_path.as_deref().unwrap_or("rubberdux");
    let binary_quoted = shell_quote(binary);
    let mut cmd = format!(
        "{} --agent --rpc-host {}:{}",
        binary_quoted, config.host_ip, config.rpc_port
    );
    if let Some(tid) = task_id {
        cmd.push_str(&format!(" --task-id {}", shell_quote(tid)));
    }

    // Ensure the binary is executable and strip quarantine attributes (macOS)
    let mut setup = if config.agent_binary_path.is_some() {
        format!(
            "chmod +x {} && xattr -d com.apple.quarantine {} 2>/dev/null || true && ",
            binary_quoted, binary_quoted
        )
    } else {
        String::new()
    };

    // Set up persistent data directory symlinks inside the VM
    if config.agent_data_dir.is_some() {
        setup.push_str(
            "OS=\"$(uname -s)\"; \
            if [[ \"$OS\" == \"Darwin\" ]]; then \
                mkdir -p \"/Volumes/My Shared Files/data/\"{documents,downloads,config,sessions,tool-results,subagents}; \
                ln -sf \"/Volumes/My Shared Files/data/documents\" ~/Documents; \
                ln -sf \"/Volumes/My Shared Files/data/downloads\" ~/Downloads; \
                ln -sf \"/Volumes/My Shared Files/data/config\" ~/.rubberdux; \
                export RUBBERDUX_DATA_DIR=\"/Volumes/My Shared Files/data\"; \
            elif [[ \"$OS\" == \"Linux\" ]]; then \
                sudo mkdir -p /mnt/shared; \
                sudo mount -t virtiofs com.apple.virtio-fs.automount /mnt/shared 2>/dev/null || true; \
                mkdir -p /mnt/shared/data/{documents,downloads,config,sessions,tool-results,subagents}; \
                ln -sf /mnt/shared/data/documents ~/Documents; \
                ln -sf /mnt/shared/data/downloads ~/Downloads; \
                ln -sf /mnt/shared/data/config ~/.rubberdux; \
                export RUBBERDUX_DATA_DIR=\"/mnt/shared/data\"; \
            fi && "
        );
    }

    let cmd = setup + &cmd;

    if config.agent_env.is_empty() {
        format!("nohup {} > /tmp/rubberdux-agent.log 2>&1 &", cmd)
    } else {
        let exports: Vec<String> = config
            .agent_env
            .iter()
            .map(|(k, v)| format!("export {}={}", shell_quote(k), shell_quote(v)))
            .collect();
        let script = exports.join(" && ") + " && " + &cmd;
        format!(
            "nohup bash -c {} > /tmp/rubberdux-agent.log 2>&1 &",
            shell_quote(&script)
        )
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Run rubberdux in host mode.
///
/// The host runs the AgentLoop locally and bridges Telegram ↔ AgentLoop.
pub async fn run(
    _config: HostConfig,
    telegram_rx: mpsc::Receiver<ChannelEvent>,
    telegram_response_tx: mpsc::Sender<AgentResponse>,
) {
    use crate::agent::builder::AgentLoopBuilder;
    use crate::agent::runtime::port::LoopEvent;
    use crate::provider::moonshot::{Message, UserContent};

    // Build local AgentLoop
    let data_dir = std::env::var("RUBBERDUX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./sessions"));

    let _ = std::fs::create_dir_all(&data_dir);

    let prompt_dir = crate::hardened_prompts::prompt_dir();
    let prompt_parts = crate::hardened_prompts::load_prompt_parts(&prompt_dir);
    let system_prompt = crate::hardened_prompts::compose_system_prompt(&prompt_parts, None);

    let client = Arc::new(crate::provider::moonshot::MoonshotClient::from_env());

    let builder = AgentLoopBuilder::new(system_prompt, data_dir);
    let (agent_loop, input_port, _context_tx) = builder.build(client);

    // Spawn AgentLoop
    tokio::spawn(async move {
        agent_loop.run().await;
    });

    // Track reply channels from Telegram adapter
    let reply_senders: Arc<Mutex<HashMap<Option<i32>, mpsc::Sender<AgentResponse>>>>
        = Arc::new(Mutex::new(HashMap::new()));

    // Bridge: Telegram → AgentLoop
    let reply_senders_for_telegram = reply_senders.clone();
    let mut telegram_rx = telegram_rx;
    tokio::spawn(async move {
        while let Some(event) = telegram_rx.recv().await {
            match event {
                ChannelEvent::UserInput {
                    interpreted,
                    telegram_message_id,
                    reply_tx,
                } => {
                    if let Some(tx) = reply_tx {
                        reply_senders_for_telegram
                            .lock()
                            .await
                            .insert(telegram_message_id, tx);
                    }

                    log::info!(
                        "Received message: msg_id={:?}, text_len={}",
                        telegram_message_id,
                        interpreted.text.len()
                    );

                    let (loop_reply_tx, mut loop_reply_rx) = mpsc::channel(8);

                    // Spawn response handler → Telegram
                    let reply_senders_clone = reply_senders_for_telegram.clone();
                    let telegram_response_tx = telegram_response_tx.clone();
                    let telegram_message_id = telegram_message_id;
                    tokio::spawn(async move {
                        while let Some(output) = loop_reply_rx.recv().await {
                            let response = AgentResponse {
                                text: output.text,
                                entry_id: output.entry_id,
                                is_final: output.is_final,
                                reply_to_message_id: telegram_message_id,
                            };
                            if let Some(tx) = reply_senders_clone.lock().await.get(&telegram_message_id) {
                                let _ = tx.send(response.clone()).await;
                            }
                            if telegram_response_tx.send(response).await.is_err() {
                                break;
                            }
                        }
                    });

                    let message = Message::User {
                        content: UserContent::Text(interpreted.text),
                    };
                    let metadata: Option<Box<dyn std::any::Any + Send>>
                        = telegram_message_id.map(|id| Box::new(id) as _);

                    let event = LoopEvent::UserMessage {
                        message,
                        reply: Some(loop_reply_tx),
                        metadata,
                    };
                    if input_port.send(event).await.is_err() {
                        log::warn!("AgentLoop input closed");
                        break;
                    }
                }
                ChannelEvent::ContextUpdate { text } => {
                    log::info!(
                        "ContextUpdate received on host: text_len={}",
                        text.len()
                    );
                    // Optionally inject as context update
                }
                ChannelEvent::InternalEvent(_) => {
                    // Internal events stay on the host side
                }
            }
        }
    });

    // Keep the host alive until shutdown
    log::info!("Host running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await.ok();

    log::info!("Host shutdown complete.");
}
        if let Some(cpus) = config.cpu_count {
            mgr = mgr.with_cpu_count(cpus);
        }
        Arc::new(Mutex::new(mgr))
    };

    // Start RPC server
    let listener = match TcpListener::bind(("0.0.0.0", config.rpc_port)).await {
        Ok(l) => {
            let actual_port = l.local_addr().unwrap().port();
            log::info!("RPC server listening on port {}", actual_port);
            l
        }
        Err(e) => {
            log::error!("Failed to bind RPC port {}: {}", config.rpc_port, e);
            return;
        }
    };

    let actual_rpc_port = listener.local_addr().unwrap().port();
    let mut config = config;
    config.rpc_port = actual_rpc_port;

    // Boot main VM
    let _main_ip = {
        let mut mgr = manager.lock().await;
        match mgr.create_and_start("main", None, config.agent_data_dir.as_deref()).await {
            Ok(ip) => {
                log::info!("Main VM booted at {}", ip);
                ip
            }
            Err(e) => {
                log::error!("Failed to boot main VM: {}", e);
                return;
            }
        }
    };

    // Wait for SSH in main VM
    {
        let mgr = manager.lock().await;
        if let Err(e) = mgr.wait_for_ssh("main").await {
            log::error!("Main VM SSH not ready: {}", e);
            return;
        }
    }

    // Copy latest agent binary to VM (ensures updates are deployed without rebuilding base VM)
    {
        let mgr = manager.lock().await;
        if let Err(e) = mgr.copy_agent_binary("main").await {
            log::warn!("Failed to copy agent binary to main VM: {}", e);
        }
    }

    // Start rubberdux --agent inside the main VM
    let agent_cmd = build_agent_command(&config, None);
    {
        let mgr = manager.lock().await;
        match mgr.exec("main", &agent_cmd).await {
            Ok(result) => {
                if result.exit_code != 0 {
                    log::warn!(
                        "Agent launch returned exit {}: {}",
                        result.exit_code, result.stderr
                    );
                }
                log::info!("Started agent inside main VM");
            }
            Err(e) => {
                log::error!("Failed to start agent in main VM: {}", e);
                return;
            }
        }
    }

    // Accept the main VM's RPC connection
    log::info!("Waiting for main VM agent to connect...");
    let main_stream = match listener.accept().await {
        Ok((stream, addr)) => {
            log::info!("Main VM agent connected from {}", addr);
            stream
        }
        Err(e) => {
            log::error!("Failed to accept main VM connection: {}", e);
            return;
        }
    };

    let (main_reader, main_writer) = main_stream.into_split();
    let main_writer = Arc::new(Mutex::new(main_writer));

    // Track child VM connections: task_id → writer
    let child_writers: Arc<Mutex<HashMap<String, Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Track reply channels from Telegram adapter so VM responses can be routed back.
    // Key: telegram_message_id -> reply channel for that specific message.
    let reply_senders: Arc<Mutex<HashMap<Option<i32>, mpsc::Sender<AgentResponse>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Spawn: Telegram -> Main VM bridge
    let writer_for_telegram = main_writer.clone();
    let reply_senders_for_telegram = reply_senders.clone();
    let mut telegram_rx = telegram_rx;
    tokio::spawn(async move {
        while let Some(event) = telegram_rx.recv().await {
            match event {
                ChannelEvent::UserInput {
                    interpreted,
                    telegram_message_id,
                    reply_tx,
                } => {
                    // Store the reply channel keyed by message_id so VM responses
                    // can be routed back to the correct Telegram message.
                    if let Some(tx) = reply_tx {
                        reply_senders_for_telegram
                            .lock()
                            .await
                            .insert(telegram_message_id, tx);
                    }
                    log::info!(
                        "Forwarding message to VM: msg_id={:?}, text_len={}",
                        telegram_message_id,
                        interpreted.text.len()
                    );
                    let msg = HostToAgent::UserMessage {
                        text: interpreted.text,
                        telegram_message_id,
                    };
                    let mut w = writer_for_telegram.lock().await;
                    match protocol::write_message(&mut w, &msg).await {
                        Ok(_) => log::info!("Message forwarded successfully"),
                        Err(e) => {
                            log::error!("Failed to forward to main VM: {}", e);
                            break;
                        }
                    }
                }
                ChannelEvent::ContextUpdate { text } => {
                    log::info!(
                        "ContextUpdate received on host (test-only feature): text_len={}",
                        text.len()
                    );
                    // ContextUpdate is a test-only feature for batched user messages.
                    // In production host mode, we ignore it since the VM agent
                    // handles user messages individually via UserInput events.
                }
                ChannelEvent::InternalEvent(_) => {
                    // Internal events stay on the host side for now
                }
            }
        }
    });

    // Spawn: Main VM -> Host message handler
    let manager_for_reader = manager.clone();
    let _child_writers = child_writers.clone();
    let listener_arc = Arc::new(listener);
    let listener_for_spawn = listener_arc.clone();
    let main_writer_for_response = main_writer.clone();
    let config_for_spawn = config.clone();
    let reply_senders_for_vm = reply_senders.clone();
    tokio::spawn(async move {
        let mut reader = main_reader;
        loop {
            let msg: Option<AgentToHost> = match protocol::read_message(&mut reader).await {
                Ok(m) => m,
                Err(e) => {
                    log::error!("RPC read error from main VM: {}", e);
                    break;
                }
            };

            let msg = match msg {
                Some(m) => m,
                None => {
                    log::info!("Main VM disconnected");
                    break;
                }
            };

            match msg {
                AgentToHost::Response {
                    text,
                    entry_id,
                    is_final,
                    reply_to_message_id,
                } => {
                    log::info!(
                        "Received response from VM: entry_id={}, is_final={}, reply_to={:?}, text_len={}",
                        entry_id,
                        is_final,
                        reply_to_message_id,
                        text.len()
                    );
                    let response = AgentResponse {
                        text,
                        entry_id,
                        is_final,
                        reply_to_message_id,
                    };
                    // Route response back to the Telegram adapter's reply channel
                    // using the message_id as the key.
                    if let Some(tx) = reply_senders_for_vm
                        .lock()
                        .await
                        .get(&reply_to_message_id)
                    {
                        log::info!("Routing response to Telegram reply channel for msg_id={:?}", reply_to_message_id);
                        let _ = tx.send(response.clone()).await;
                    } else {
                        log::warn!(
                            "No reply channel found for message_id={:?}, dropping response",
                            reply_to_message_id
                        );
                    }
                    // Also send to the debug/legacy channel
                    if telegram_response_tx.send(response).await.is_err() {
                        log::warn!("Telegram response channel closed");
                        break;
                    }
                }
                AgentToHost::SpawnVM {
                    task_id,
                    prompt,
                    subagent_type,
                } => {
                    log::info!("Main VM requested child VM: {}", task_id);
                    let mgr = manager_for_reader.clone();
                    let tid = task_id.clone();
                    let sub_ty = subagent_type.clone();
                    let writer = main_writer_for_response.clone();
                    let listener = listener_for_spawn.clone();
                    let cfg = config_for_spawn.clone();

                    tokio::spawn(async move {
                        let result =
                            run_child_vm(mgr, &tid, &prompt, &sub_ty, &cfg, listener).await;

                        let response = match result {
                            Ok(summary) => HostToAgent::VMCompleted {
                                task_id: tid,
                                result: summary,
                            },
                            Err(e) => HostToAgent::VMFailed {
                                task_id: tid,
                                error: e.to_string(),
                            },
                        };

                        let mut w = writer.lock().await;
                        if let Err(e) = protocol::write_message(&mut w, &response).await {
                            log::error!("Failed to send VM result to main agent: {}", e);
                        }
                    });
                }
            }
        }
    });

    // Keep the host alive until shutdown
    log::info!("Host running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await.ok();

    log::info!("Shutting down VMs...");
    let mut mgr = manager.lock().await;
    mgr.destroy_all().await;
    log::info!("Host shutdown complete.");
}

/// Run a child VM to completion and return the final output.
/// Guarantees the child VM is destroyed even if the agent fails or errors occur.
async fn run_child_vm(
    manager: Arc<Mutex<VMManager>>,
    task_id: &str,
    prompt: &str,
    subagent_type: &str,
    config: &HostConfig,
    listener: Arc<TcpListener>,
) -> Result<String, Error> {

    // Helper to write status updates to the child share for debugging
    async fn write_status(share_dir: &std::path::Path, msg: &str) {
        let _ = tokio::fs::write(share_dir.join("status.txt"), msg).await;
    }

    // Create and start child VM
    {
        let mut mgr = manager.lock().await;
        write_status(&mgr.share_dir(task_id), "run_child_vm: creating VM").await;
        mgr.create_and_start(task_id, None, config.agent_data_dir.as_deref()).await?;
    }

    // Run the child VM lifecycle with guaranteed cleanup
    let result = async {
        // Wait for SSH
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: waiting for SSH").await;
            mgr.wait_for_ssh(task_id).await?;
        }

        // Copy the agent binary from the main VM share to the child VM share
        // so the child can execute it.
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: copying binary and prompt").await;
            let main_binary = config.share_root.join("main").join("rubberdux");
            let child_binary = mgr.share_dir(task_id).join("rubberdux");
            if main_binary.exists() {
                tokio::fs::copy(&main_binary, &child_binary).await?;
            }
            let prompt_path = mgr.share_dir(task_id).join("prompt.txt");
            tokio::fs::write(&prompt_path, prompt).await?;
            let subagent_type_path = mgr.share_dir(task_id).join("subagent_type.txt");
            tokio::fs::write(&subagent_type_path, subagent_type).await?;
        }

        // Start the agent inside the child VM
        let agent_cmd = build_agent_command(config, Some(task_id));
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: starting agent").await;
            let result = mgr.exec(task_id, &agent_cmd).await?;
            if result.exit_code != 0 {
                let err = format!(
                    "Child VM agent failed to start (exit {}): stdout={} stderr={}",
                    result.exit_code, result.stdout, result.stderr
                );
                write_status(&mgr.share_dir(task_id), &err).await;
                return Err(Error::Vm(err));
            }
        }

        // Copy child VM agent log to share immediately so it survives even if
        // listener.accept() hangs (helps debugging connection issues).
        {
            let mgr = manager.lock().await;
            let log_result = mgr
                .exec(task_id, "cat /tmp/rubberdux-agent.log 2>/dev/null || true")
                .await;
            let early_log = log_result.map(|r| r.stdout).unwrap_or_default();
            let log_path = mgr.share_dir(task_id).join("agent.log");
            let _ = tokio::fs::write(&log_path, &early_log).await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: waiting for RPC connection").await;
        }

        // Accept the child's RPC connection
        // TODO: proper connection routing by task_id instead of accept order
        let (stream, addr) = listener.accept().await?;
        log::info!("Child VM {} connected from {}", task_id, addr);

        let (mut reader, _writer) = stream.into_split();

        // Read messages until the child sends its final response
        let mut final_text = String::new();
        loop {
            let msg: Option<AgentToHost> = protocol::read_message(&mut reader).await?;
            match msg {
                Some(AgentToHost::Response {
                    text, is_final, ..
                }) => {
                    final_text = text;
                    if is_final {
                        break;
                    }
                }
                Some(AgentToHost::SpawnVM { .. }) => {
                    // Defensive guard: child VMs no longer have the agent tool,
                    // so this should never happen. Log and ignore.
                    log::warn!("Child VM {} requested nested spawn (ignoring)", task_id);
                }
                None => {
                    log::info!("Child VM {} disconnected", task_id);
                    break;
                }
            }
        }

        // Copy child VM agent log to share for debugging before destruction
        {
            let mgr = manager.lock().await;
            let log_result = mgr
                .exec(task_id, "cat /tmp/rubberdux-agent.log 2>/dev/null || true")
                .await;
            let log_content = log_result.map(|r| r.stdout).unwrap_or_default();
            let log_path = mgr.share_dir(task_id).join("agent.log");
            let _ = tokio::fs::write(&log_path, &log_content).await;
            // Also persist on the host filesystem so it survives share cleanup
            let host_log_path = std::path::PathBuf::from(format!("/tmp/rubberdux-child-{}.log", task_id));
            let _ = tokio::fs::write(&host_log_path, &log_content).await;
        }

        Ok(final_text)
    }.await;

    // Destroy the child VM regardless of success or failure
    {
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.destroy(task_id).await {
            log::warn!("Failed to destroy child VM {}: {}", task_id, e);
        }
    }

    result
}
