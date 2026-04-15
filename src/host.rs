use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

use crate::channel::{AgentResponse, ChannelEvent};
use crate::error::Error;
use crate::vm::manager::VMManager;
use crate::vm::rpc::{self, AgentToHost, HostToAgent};

const DEFAULT_RPC_PORT: u16 = 19384;

/// Run rubberdux in host mode.
///
/// The host is a thin proxy that:
/// 1. Manages Tart VM lifecycles
/// 2. Boots the main agent VM
/// 3. Bridges Telegram ↔ main VM via RPC
/// 4. Handles child VM spawn requests from any agent VM
pub async fn run(
    telegram_rx: mpsc::Receiver<ChannelEvent>,
    telegram_response_tx: mpsc::Sender<AgentResponse>,
) {
    let image = std::env::var("RUBBERDUX_VM_IMAGE")
        .unwrap_or_else(|_| "macos-sequoia-xcode".to_string());

    let share_root = std::env::var("RUBBERDUX_VM_SHARES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./vm-shares"));

    let rpc_port: u16 = std::env::var("RUBBERDUX_RPC_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RPC_PORT);

    let manager = Arc::new(Mutex::new(VMManager::new(image, share_root)));

    // Start RPC server
    let listener = match TcpListener::bind(("0.0.0.0", rpc_port)).await {
        Ok(l) => {
            log::info!("RPC server listening on port {}", rpc_port);
            l
        }
        Err(e) => {
            log::error!("Failed to bind RPC port {}: {}", rpc_port, e);
            return;
        }
    };

    // Boot main VM
    let _main_ip = {
        let mut mgr = manager.lock().await;
        match mgr.create_and_start("main", None).await {
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

    // Start rubberdux --agent inside the main VM
    let host_ip = "192.168.64.1"; // default gateway IP seen from Tart guests
    let agent_cmd = format!(
        "rubberdux --agent --rpc-host {}:{}",
        host_ip, rpc_port
    );
    {
        let mgr = manager.lock().await;
        // Launch agent in background inside VM
        let bg_cmd = format!("nohup {} > /tmp/rubberdux-agent.log 2>&1 &", agent_cmd);
        match mgr.exec("main", &bg_cmd).await {
            Ok(result) => {
                if result.exit_code != 0 {
                    log::warn!(
                        "Agent launch returned exit {}: {}",
                        result.exit_code,
                        result.stderr
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

    // Spawn: Telegram → Main VM bridge
    let writer_for_telegram = main_writer.clone();
    let mut telegram_rx = telegram_rx;
    tokio::spawn(async move {
        while let Some(event) = telegram_rx.recv().await {
            match event {
                ChannelEvent::UserInput {
                    interpreted,
                    telegram_message_id,
                    ..
                } => {
                    let msg = HostToAgent::UserMessage {
                        text: interpreted.text,
                        telegram_message_id,
                    };
                    let mut w = writer_for_telegram.lock().await;
                    if let Err(e) = rpc::write_message(&mut w, &msg).await {
                        log::error!("Failed to forward to main VM: {}", e);
                        break;
                    }
                }
                ChannelEvent::InternalEvent(_) => {
                    // Internal events stay on the host side for now
                }
            }
        }
    });

    // Spawn: Main VM → Host message handler
    let manager_for_reader = manager.clone();
    let _child_writers = child_writers.clone();
    let listener_arc = Arc::new(listener);
    let listener_for_spawn = listener_arc.clone();
    let main_writer_for_response = main_writer.clone();
    tokio::spawn(async move {
        let mut reader = main_reader;
        loop {
            let msg: Option<AgentToHost> = match rpc::read_message(&mut reader).await {
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
                    let response = AgentResponse {
                        text,
                        entry_id,
                        is_final,
                        reply_to_message_id,
                    };
                    if telegram_response_tx.send(response).await.is_err() {
                        log::warn!("Telegram response channel closed");
                        break;
                    }
                }
                AgentToHost::SpawnVM { task_id, prompt } => {
                    log::info!("Main VM requested child VM: {}", task_id);
                    let mgr = manager_for_reader.clone();
                    let tid = task_id.clone();
                    let writer = main_writer_for_response.clone();
                    let listener = listener_for_spawn.clone();
                    let rpc_port_inner = DEFAULT_RPC_PORT;

                    tokio::spawn(async move {
                        let result =
                            run_child_vm(mgr, &tid, &prompt, host_ip, rpc_port_inner, listener)
                                .await;

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
                        if let Err(e) = rpc::write_message(&mut w, &response).await {
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
async fn run_child_vm(
    manager: Arc<Mutex<VMManager>>,
    task_id: &str,
    prompt: &str,
    host_ip: &str,
    rpc_port: u16,
    listener: Arc<TcpListener>,
) -> Result<String, Error> {
    // Create and start child VM
    {
        let mut mgr = manager.lock().await;
        mgr.create_and_start(task_id, None).await?;
    }

    // Wait for SSH
    {
        let mgr = manager.lock().await;
        mgr.wait_for_ssh(task_id).await?;
    }

    // Write the prompt to the share directory so the agent can read it
    {
        let mgr = manager.lock().await;
        let prompt_path = mgr.share_dir(task_id).join("prompt.txt");
        tokio::fs::write(&prompt_path, prompt).await?;
    }

    // Start the agent inside the child VM
    let agent_cmd = format!(
        "rubberdux --agent --rpc-host {}:{} --task-id {}",
        host_ip, rpc_port, task_id
    );
    {
        let mgr = manager.lock().await;
        let bg_cmd = format!("nohup {} > /tmp/rubberdux-agent.log 2>&1 &", agent_cmd);
        mgr.exec(task_id, &bg_cmd).await?;
    }

    // Accept the child's RPC connection
    // TODO: proper connection routing by task_id instead of accept order
    let (stream, addr) = listener.accept().await?;
    log::info!("Child VM {} connected from {}", task_id, addr);

    let (mut reader, _writer) = stream.into_split();

    // Read messages until the child sends its final response
    let mut final_text = String::new();
    loop {
        let msg: Option<AgentToHost> = rpc::read_message(&mut reader).await?;
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
                // Nested spawn from child — could recurse, but for now
                // we don't support it. Return error to the child.
                log::warn!("Child VM {} requested nested spawn (not supported)", task_id);
            }
            None => {
                log::info!("Child VM {} disconnected", task_id);
                break;
            }
        }
    }

    // Destroy the child VM
    {
        let mut mgr = manager.lock().await;
        mgr.destroy(task_id).await?;
    }

    Ok(final_text)
}
