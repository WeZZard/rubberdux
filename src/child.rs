use std::path::PathBuf;

use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::agent::external::{ExternalAgentEvent, UIInteractionResponse};

fn find_share_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Volumes/My Shared Files")
    } else {
        PathBuf::from("/mnt/shared")
    }
}

/// Run the native app-worker path: a real `AgentLoop` for a single App, bridged
/// to the host over RPC. Delegates to
/// [`crate::app::runtime::worker::run_app_worker`], which sends `Hello{app_id}`
/// first and then streams entries. See `docs/app/runtime/worker-lifecycle.md`.
pub async fn run_app_child(rpc_host: String, app_id: String, app_session_dir: PathBuf) {
    crate::app::runtime::worker::run_app_worker(rpc_host, app_id, app_session_dir).await;
}

pub async fn run_child(rpc_host: String, task_id: String) {
    let share_dir = find_share_dir();

    let prompt = match tokio::fs::read_to_string(share_dir.join("prompt.txt")).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("Failed to read prompt.txt from share dir: {}", e);
            return;
        }
    };

    let subagent_type = match tokio::fs::read_to_string(share_dir.join("subagent_type.txt")).await {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log::error!("Failed to read subagent_type.txt from share dir: {}", e);
            return;
        }
    };

    let agent_name = tokio::fs::read_to_string(share_dir.join("agent_name.txt"))
        .await
        .ok()
        .map(|s| s.trim().to_string());

    let stream = match TcpStream::connect(&rpc_host).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("Failed to connect to host at {}: {}", rpc_host, e);
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    if subagent_type == "external" {
        run_external_task(
            &task_id,
            &prompt,
            agent_name.as_deref(),
            &mut reader,
            &mut writer,
        )
        .await;
    } else {
        let msg = AgentToHost::Response {
            text: format!(
                "Child agent mode for '{}' not yet implemented",
                subagent_type
            ),
            entry_id: 0,
            is_final: true,
            reply_to_message_id: None,
        };
        if let Err(e) = protocol::write_message(&mut writer, &msg).await {
            log::error!("Failed to send unimplemented-mode response: {}", e);
        }
    }
}

async fn run_external_task(
    task_id: &str,
    prompt: &str,
    agent_name: Option<&str>,
    reader: &mut OwnedReadHalf,
    writer: &mut OwnedWriteHalf,
) {
    let name = agent_name.unwrap_or("claude_code");
    let (event_tx, mut event_rx) = mpsc::channel::<ExternalAgentEvent>(32);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));

    let response_tx = match name {
        "claude_code" => {
            match crate::agent::external::claude_code::ClaudeCodeSession::spawn(
                prompt, &cwd, event_tx,
            )
            .await
            {
                Ok((session, tx)) => {
                    tokio::spawn(session.drive());
                    tx
                }
                Err(e) => {
                    log::error!("Failed to spawn Claude Code session: {}", e);
                    let msg = AgentToHost::Response {
                        text: format!("Failed to spawn Claude Code session: {}", e),
                        entry_id: 0,
                        is_final: true,
                        reply_to_message_id: None,
                    };
                    let _ = protocol::write_message(writer, &msg).await;
                    return;
                }
            }
        }
        "codex" => {
            match crate::agent::external::codex::CodexSession::spawn(prompt, &cwd, event_tx).await
            {
                Ok((session, tx)) => {
                    tokio::spawn(session.drive());
                    tx
                }
                Err(e) => {
                    log::error!("Failed to spawn Codex session: {}", e);
                    let msg = AgentToHost::Response {
                        text: format!("Failed to spawn Codex session: {}", e),
                        entry_id: 0,
                        is_final: true,
                        reply_to_message_id: None,
                    };
                    let _ = protocol::write_message(writer, &msg).await;
                    return;
                }
            }
        }
        other => {
            log::error!("Unknown external agent name: {}", other);
            let msg = AgentToHost::Response {
                text: format!("Unknown external agent name: {}", other),
                entry_id: 0,
                is_final: true,
                reply_to_message_id: None,
            };
            let _ = protocol::write_message(writer, &msg).await;
            return;
        }
    };

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(ExternalAgentEvent::UIInteraction(request)) => {
                        let msg = AgentToHost::ExternalInteraction {
                            task_id: task_id.into(),
                            request,
                        };
                        if let Err(e) = protocol::write_message(writer, &msg).await {
                            log::error!("Failed to send ExternalInteraction: {}", e);
                            break;
                        }
                    }
                    Some(ExternalAgentEvent::Completed { result }) => {
                        let msg = AgentToHost::Response {
                            text: result,
                            entry_id: 0,
                            is_final: true,
                            reply_to_message_id: None,
                        };
                        if let Err(e) = protocol::write_message(writer, &msg).await {
                            log::error!("Failed to send completion response: {}", e);
                        }
                        break;
                    }
                    Some(ExternalAgentEvent::Failed { error }) => {
                        let msg = AgentToHost::Response {
                            text: format!("External agent failed: {}", error),
                            entry_id: 0,
                            is_final: true,
                            reply_to_message_id: None,
                        };
                        if let Err(e) = protocol::write_message(writer, &msg).await {
                            log::error!("Failed to send failure response: {}", e);
                        }
                        break;
                    }
                    Some(ExternalAgentEvent::Progress { message }) => {
                        log::info!("[child:{}] {}", task_id, message);
                    }
                    None => {
                        log::info!("[child:{}] event channel closed", task_id);
                        break;
                    }
                }
            }
            host_msg = protocol::read_message::<HostToAgent>(reader) => {
                match host_msg {
                    Ok(Some(HostToAgent::InteractionResponse { response, .. })) => {
                        if let Err(e) = response_tx.send(response).await {
                            log::warn!("[child:{}] Failed to forward interaction response: {}", task_id, e);
                        }
                    }
                    Ok(Some(HostToAgent::Shutdown)) => {
                        log::info!("[child:{}] received shutdown", task_id);
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        log::info!("[child:{}] host disconnected", task_id);
                        break;
                    }
                    Err(e) => {
                        log::error!("[child:{}] error reading from host: {}", task_id, e);
                        break;
                    }
                }
            }
        }
    }
}
