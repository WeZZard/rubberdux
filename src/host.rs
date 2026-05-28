use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use teloxide::prelude::Bot;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::error::Error;
use crate::protocol::{self, AgentToHost};
use crate::vm::manager::VMManager;

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

        let host_ip =
            std::env::var("RUBBERDUX_HOST_IP").unwrap_or_else(|_| "192.168.64.1".to_string());

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
    let binary = config.agent_binary_path.as_deref().unwrap_or("rubberduxd");
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
/// The host runs the AgentLoop locally and bridges Telegram ↔ AgentLoop
/// via the broadcast-based adapter.
pub async fn run(_config: HostConfig, bot: Bot) {
    use crate::agent::builder::AgentLoopBuilder;

    // Initialize session manager and create new session
    let session_manager = Arc::new(crate::session::SessionManager::new());
    let model = std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());
    let (session_id, session_dir) = session_manager
        .create_session(model)
        .expect("Failed to create session");

    log::info!(
        "Created session: {} at {}",
        session_id.to_string(),
        session_dir.display()
    );

    // Create project root symlink if missing
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !project_root.join("sessions").exists() {
        if let Err(e) = crate::session::SessionManager::create_project_symlink(&project_root) {
            log::warn!("Failed to create project sessions symlink: {}", e);
        }
    }

    let mindset = Arc::new(crate::mindset::Mindset::new());
    mindset.ensure_dirs().expect("Failed to initialize mindset");
    mindset.seed_defaults_if_empty(&project_root.join("prompts"));
    log::info!("Mindset root: {}", mindset.root.display());

    let workspace = Arc::new(crate::workspace::Workspace::new());
    workspace.ensure_dirs().expect("Failed to initialize workspace");

    log::info!("Workspace root: {}", workspace.root.display());

    let interaction_queue = std::sync::Arc::new(
        crate::agent::external::interaction_queue::InteractionQueue::new(),
    );

    let mut conventions = crate::guardrail::ConventionRegistry::new();
    conventions.register(crate::channel::adapter::telegram_guardrails::convention());
    conventions.register(crate::workspace::convention());
    conventions.register(crate::mindset::convention());
    conventions.register(crate::agent::external::convention(interaction_queue.clone()));

    let convention_guidance = conventions.compose_guidance();
    let guardrails = conventions.build_guardrail_chain();

    let mut prompt_parts = crate::hardened_prompts::load_prompt_parts(&mindset.root);
    prompt_parts.push(convention_guidance);
    let channel_partial = Some(crate::channel::adapter::telegram::channel_prompt());
    let system_prompt =
        crate::hardened_prompts::compose_system_prompt(&prompt_parts, channel_partial);

    let client = Arc::new(crate::provider::moonshot::MoonshotClient::from_env());

    // Create the trajectory broadcast channel and recorder before building the
    // agent loop so that events flow to both the filesystem log and any
    // WebSocket subscribers on `/api/v1/ws/trajectory`.
    let (trajectory_tx, _) = tokio::sync::broadcast::channel(256);
    let events_path = session_manager
        .main_agent_dir(&session_id)
        .join("events.jsonl");
    let gateway_events_path = events_path.clone();
    let fs_recorder = crate::trajectory::filesystem_recorder(events_path);
    let broadcast_recorder: crate::trajectory::SharedTrajectoryRecorder = Arc::new(
        crate::trajectory::BroadcastTrajectoryRecorder::new(
            fs_recorder,
            trajectory_tx.clone(),
        ),
    );

    let gateway_system_prompt = system_prompt.clone();
    let telegram_chat_id: std::sync::Arc<tokio::sync::Mutex<Option<i64>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let telegram_processor = std::sync::Arc::new(
        crate::channel::adapter::telegram::TelegramChannelProcessor::new(
            bot.clone(),
            telegram_chat_id.clone(),
        )
        .with_interaction_queue(interaction_queue.clone()),
    );
    let builder = AgentLoopBuilder::new(system_prompt, session_manager)
        .with_session_id(session_id)
        .with_workspace(workspace)
        .with_mindset(mindset.clone())
        .with_channel_processor("telegram", telegram_processor)
        .with_guardrails(guardrails)
        .with_recorder(broadcast_recorder)
        .with_external_cwd(project_root.clone())
        .with_interaction_queue(interaction_queue.clone());
    let (agent_loop, input_port, _context_tx) = builder.build(client).await;

    // Subscribe to entry broadcasts for the Telegram adapter
    let entry_rx = agent_loop.subscribe_output().into_receiver();

    // Set up the gateway server
    let _gateway_handle = {
        let output_port = agent_loop.subscribe_output();
        let identity = std::fs::read_to_string(mindset.identity_path()).unwrap_or_default();
        let soul = std::fs::read_to_string(mindset.soul_path()).unwrap_or_default();
        let gateway_state = Arc::new(crate::gateway::state::GatewayState::with_trajectory_tx(
            gateway_system_prompt, identity, soul, trajectory_tx, input_port.clone(),
            Some(gateway_events_path),
        ));
        let state_clone = gateway_state.clone();
        tokio::spawn(crate::gateway::stream::mirror_entries(output_port, state_clone));
        tokio::spawn(crate::gateway::server::run(gateway_state))
    };

    // Spawn AgentLoop
    tokio::spawn(async move {
        agent_loop.run().await;
    });

    // Run Telegram adapter (blocks until dispatcher shuts down)
    crate::channel::adapter::telegram::run(bot, input_port, entry_rx, telegram_chat_id, interaction_queue).await;

    log::info!("Host shutdown complete.");
}

/// Run a child VM to completion and return the final output.
/// Guarantees the child VM is destroyed even if the agent fails or errors occur.
pub async fn run_child_vm(
    manager: Arc<Mutex<VMManager>>,
    task_id: &str,
    prompt: &str,
    subagent_type: &str,
    config: &HostConfig,
    listener: Arc<TcpListener>,
    interaction_queue: std::sync::Arc<crate::agent::external::interaction_queue::InteractionQueue>,
) -> Result<String, Error> {
    // Helper to write status updates to the child share for debugging
    async fn write_status(share_dir: &std::path::Path, msg: &str) {
        let _ = tokio::fs::write(share_dir.join("status.txt"), msg).await;
    }

    // Create and start child VM
    {
        let mut mgr = manager.lock().await;
        write_status(&mgr.share_dir(task_id), "run_child_vm: creating VM").await;
        mgr.create_and_start(task_id, None, config.agent_data_dir.as_deref())
            .await?;
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
            write_status(
                &mgr.share_dir(task_id),
                "run_child_vm: copying binary and prompt",
            )
            .await;
            let main_binary = config.share_root.join("main").join("rubberduxd");
            let child_binary = mgr.share_dir(task_id).join("rubberduxd");
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
            write_status(
                &mgr.share_dir(task_id),
                "run_child_vm: waiting for RPC connection",
            )
            .await;
        }

        // Accept the child's RPC connection
        // TODO: proper connection routing by task_id instead of accept order
        let (stream, addr) = listener.accept().await?;
        log::info!("Child VM {} connected from {}", task_id, addr);

        let (mut reader, writer) = stream.into_split();
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

        // Read messages until the child sends its final response
        let mut final_text = String::new();
        loop {
            let msg: Option<AgentToHost> = protocol::read_message(&mut reader).await?;
            match msg {
                Some(AgentToHost::Response { text, is_final, .. }) => {
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
                Some(AgentToHost::ExternalInteraction { task_id: _ext_task_id, request }) => {
                    let request_id = crate::agent::external::get_request_id(&request).to_string();
                    log::info!("VM child {} sent ExternalInteraction: {}", task_id, request_id);

                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    interaction_queue.add(
                        request_id.clone(),
                        crate::agent::external::interaction_queue::PendingInteraction {
                            request,
                            response_tx: resp_tx,
                        },
                    );

                    // Spawn task to write response back to VM when interaction is resolved
                    let writer_clone = writer.clone();
                    tokio::spawn(async move {
                        if let Ok(response) = resp_rx.await {
                            let msg = crate::protocol::HostToAgent::InteractionResponse {
                                request_id,
                                response,
                            };
                            let mut w = writer_clone.lock().await;
                            if let Err(e) = crate::protocol::write_message(&mut *w, &msg).await {
                                log::warn!("Failed to send InteractionResponse to VM: {}", e);
                            }
                        }
                    });
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
            let host_log_path =
                std::path::PathBuf::from(format!("/tmp/rubberdux-child-{}.log", task_id));
            let _ = tokio::fs::write(&host_log_path, &log_content).await;
        }

        Ok(final_text)
    }
    .await;

    // Destroy the child VM regardless of success or failure
    {
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.destroy(task_id).await {
            log::warn!("Failed to destroy child VM {}: {}", task_id, e);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct EnvVarGuard {
        key: &'static str,
        value: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, value }
        }

        fn set(key: &'static str, new_value: &str) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, new_value);
            }
            Self { key, value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.value {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[test]
    #[serial(host_config_env)]
    fn test_host_config_from_env_defaults() {
        let _guard = EnvVarGuard::unset("RUBBERDUX_RPC_PORT");
        // Test that HostConfig::from_env() doesn't panic when env vars are missing
        // by setting required vars if not present
        let config = HostConfig::from_env();
        assert_eq!(config.rpc_port, DEFAULT_RPC_PORT);
    }

    #[test]
    #[serial(host_config_env)]
    fn test_host_config_custom_rpc_port() {
        let _guard = EnvVarGuard::set("RUBBERDUX_RPC_PORT", "12345");
        let config = HostConfig::from_env();
        assert_eq!(config.rpc_port, 12345);
    }

    #[test]
    fn test_build_agent_command_basic() {
        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: HashMap::new(),
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, None);
        assert!(cmd.contains("rubberduxd"));
        assert!(cmd.contains("--agent"));
        assert!(cmd.contains("192.168.64.1:19384"));
    }

    #[test]
    fn test_build_agent_command_with_task_id() {
        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: HashMap::new(),
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, Some("task-123"));
        assert!(cmd.contains("--task-id"));
        assert!(cmd.contains("task-123"));
    }

    #[test]
    fn test_build_agent_command_with_env() {
        let mut env = HashMap::new();
        env.insert("TEST_KEY".to_string(), "test_value".to_string());

        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: env,
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, None);
        assert!(cmd.contains("TEST_KEY"));
        assert!(cmd.contains("test_value"));
        assert!(cmd.contains("export"));
    }
}
