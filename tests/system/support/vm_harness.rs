use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rubberdux::agent::builder::AgentLoopBuilder;
use rubberdux::agent::entry::{Entry, EntryOrigin};
use rubberdux::agent::runtime::port::{EntryNotification, InputPort};
use rubberdux::host::HostConfig;
use rubberdux::provider::kimi_for_coding::{Message, UserContent};
use rubberdux::provider::ModelApi;
use rubberdux::provider::dialect::openai_chat_completions::OpenAiChatCompletions;
use rubberdux::vm::setup::ssh_private_key;
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::harness::AgentResponse;
use super::setup::{cleanup_stale_vms, linux_agent_binary_path};

pub struct VmSystemTestHarness {
    _temp_dir: tempfile::TempDir,
    input_port: InputPort,
    entry_rx: tokio::sync::Mutex<broadcast::Receiver<EntryNotification>>,
    pub mock_server: MockServer,
    share_root: PathBuf,
    host_task: tokio::task::JoinHandle<()>,
}

impl VmSystemTestHarness {
    pub async fn new() -> Self {
        // 1. Pre-flight cleanup of stale VMs from prior crashed runs
        cleanup_stale_vms();

        // 2. Fail fast if any rubberdux VMs are still running — on Apple Silicon
        //    the hard limit for concurrent macOS VMs is 2, and a leaked VM will
        //    silently consume one slot and cause child VM tests to time out.
        let running = tokio::process::Command::new("tart")
            .args(["list"])
            .output()
            .await;
        if let Ok(out) = running {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let still_running: Vec<&str> = stdout
                .lines()
                .filter(|l| {
                    let parts: Vec<&str> = l.split_whitespace().collect();
                    parts.len() >= 3
                        && parts[1].starts_with("rubberdux-")
                        && parts.last().map_or(false, |s| *s == "running")
                })
                .collect();
            if !still_running.is_empty() {
                panic!(
                    "Leaked Tart VMs are still running and will exhaust the 2-VM concurrency limit:\n{}",
                    still_running.join("\n")
                );
            }
        }

        // 3. Get Linux binary path (must be pre-built)
        let binary_path = linux_agent_binary_path();

        // 4. Temp share directory
        let temp_dir = tempfile::tempdir().unwrap();
        let share_root = temp_dir.path().to_path_buf();
        let agent_data_dir = temp_dir.path().join("agent-data");
        tokio::fs::create_dir_all(&agent_data_dir).await.unwrap();

        // The main VM's share will be <share_root>/main; pre-create it so the binary
        // is visible inside the VM at /Volumes/My Shared Files/share/rubberdux.
        let main_share = share_root.join("main");
        tokio::fs::create_dir_all(&main_share).await.unwrap();
        tokio::fs::copy(binary_path, main_share.join("rubberdux"))
            .await
            .unwrap();

        // 6. Start wiremock for Moonshot API mock (bind to 0.0.0.0 so the VM can reach it)
        let wiremock_listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let wiremock_port = wiremock_listener.local_addr().unwrap().port();
        let mock_server = wiremock::MockServer::builder()
            .listener(wiremock_listener)
            .start()
            .await;

        // 7. HostConfig with dynamic RPC port (0 means bind to any free port)
        // Use 8 GB memory and 6 CPUs so two VMs can run side-by-side on a 64 GB host
        // without oversubscribing cores.
        let _host_config = HostConfig {
            vm_image: "rubberdux-base-ubuntu24-release".into(),
            share_root: share_root.clone(),
            rpc_port: 0,
            // 0 ⇒ bind any free port; this harness constructs HostConfig only to
            // exercise the type and never starts the surface-client listener.
            surface_port: 0,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: Some("/mnt/shared/share/rubberdux".into()),
            agent_env: [
                (
                    "RUBBERDUX_LLM_BASE_URL".into(),
                    format!("http://192.168.64.1:{}", wiremock_port),
                ),
                ("RUBBERDUX_LLM_API_KEY".into(), "test-key".into()),
                (
                    "RUBBERDUX_LLM_BEST_PERFORMANCE_TOKENS".into(),
                    "4000".into(),
                ),
            ]
            .into_iter()
            .collect(),
            agent_data_dir: Some(agent_data_dir),
            memory_mb: Some(8192),
            cpu_count: Some(6),
        };

        // 8. Create AgentLoop directly (bypassing host::run which now requires a Bot)
        let session_manager = Arc::new(rubberdux::session::SessionManager::new());
        let model = "test-model".to_string();
        let (session_id, _session_dir) = session_manager
            .create_session(model)
            .expect("Failed to create session");

        let system_prompt = "You are a test agent.".to_string();
        let client: Arc<dyn ModelApi> = Arc::new(OpenAiChatCompletions::new(
            reqwest::Client::new(),
            format!("http://127.0.0.1:{}", wiremock_port),
            "test-key".into(),
            "test-model".into(),
            rubberdux::provider::AuthScheme::Bearer,
        ));

        let builder = AgentLoopBuilder::new(system_prompt, session_manager)
            .with_session_id(session_id);
        let (agent_loop, input_port, _context_tx) = builder.build(client).await;

        let entry_rx = agent_loop.subscribe_output().into_receiver();

        // 9. Spawn AgentLoop
        let host_task = tokio::spawn(async move {
            agent_loop.run().await;
        });

        Self {
            _temp_dir: temp_dir,
            input_port,
            entry_rx: tokio::sync::Mutex::new(entry_rx),
            mock_server,
            share_root,
            host_task,
        }
    }

    pub fn share_root(&self) -> &Path {
        &self.share_root
    }

    /// Mount a wiremock response for the Moonshot chat completions endpoint.
    pub async fn mock_llm(&self, response: serde_json::Value, expected_calls: Option<u64>) {
        let mut mock = Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response));

        if let Some(n) = expected_calls {
            mock = mock.up_to_n_times(n);
        }

        mock.mount(&self.mock_server).await;
    }

    /// Dump all received wiremock requests for debugging unmatched mocks.
    pub async fn dump_wiremock_requests(&self) -> String {
        let mut out = String::new();
        if let Some(reqs) = self.mock_server.received_requests().await {
            for (i, req) in reqs.iter().enumerate() {
                out.push_str(&format!("[req {}] {} {}\n", i, req.method, req.url.path()));
                if let Ok(body) = std::str::from_utf8(&req.body) {
                    // Truncate very large bodies
                    let preview = &body[..body.len().min(2000)];
                    out.push_str("body:\n");
                    out.push_str(preview);
                    out.push('\n');
                }
                out.push('\n');
            }
        } else {
            out.push_str("(no wiremock requests recorded)\n");
        }
        out
    }

    /// Read the copied agent.log and status.txt from a child VM share directory.
    pub async fn read_child_vm_agent_log(&self) -> String {
        let entries = match tokio::fs::read_dir(&self.share_root).await {
            Ok(e) => e,
            Err(e) => return format!("Failed to read share root: {}", e),
        };

        let mut out = String::new();
        let mut entries = entries;
        let mut found = false;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "main" {
                let dir = entry.path();
                let status_path = dir.join("status.txt");
                let log_path = dir.join("agent.log");
                if status_path.exists() || log_path.exists() {
                    found = true;
                    out.push_str(&format!("--- {} ---\n", dir.display()));
                    if status_path.exists() {
                        out.push_str("status.txt:\n");
                        match tokio::fs::read_to_string(&status_path).await {
                            Ok(content) => out.push_str(&content),
                            Err(e) => out.push_str(&format!("Failed to read status: {}\n", e)),
                        }
                        out.push('\n');
                    }
                    if log_path.exists() {
                        out.push_str("agent.log:\n");
                        match tokio::fs::read_to_string(&log_path).await {
                            Ok(content) => out.push_str(&content),
                            Err(e) => out.push_str(&format!("Failed to read log: {}\n", e)),
                        }
                        out.push('\n');
                    }
                }
            }
        }

        if !found {
            // Fallback: check for host-persisted child logs
            let host_logs = match tokio::fs::read_dir("/tmp").await {
                Ok(d) => d,
                Err(_) => return "No child VM share directory found in share root".into(),
            };
            let mut host_logs = host_logs;
            let mut any = false;
            while let Ok(Some(entry)) = host_logs.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("rubberdux-child-") && name.ends_with(".log") {
                    any = true;
                    out.push_str(&format!("--- /tmp/{} ---\n", name));
                    match tokio::fs::read_to_string(entry.path()).await {
                        Ok(content) => out.push_str(&content),
                        Err(e) => out.push_str(&format!("Failed to read: {}\n", e)),
                    }
                    out.push('\n');
                }
            }
            if !any {
                return "No child VM share directory found in share root".into();
            }
        }
        out
    }

    /// Try to read the agent log from the main VM via SSH for debugging.
    pub async fn read_main_vm_agent_log(&self) -> String {
        let output = tokio::process::Command::new("tart")
            .args(["list"])
            .output()
            .await;

        let mut main_vm_name: Option<String> = None;
        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1].starts_with("rubberdux-main-") {
                    main_vm_name = Some(parts[1].to_string());
                    break;
                }
            }
        }

        let Some(vm_name) = main_vm_name else {
            return "No running main VM found".into();
        };

        let ip_output = tokio::process::Command::new("tart")
            .args(["ip", &vm_name])
            .output()
            .await;

        let ip = match ip_output {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => return format!("Failed to get IP for VM {}", vm_name),
        };

        let key_path = ssh_private_key();
        let ssh_output = tokio::process::Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=5",
                "-i",
                &key_path.to_string_lossy(),
                &format!("admin@{}", ip),
                "cat /tmp/rubberdux-agent.log 2>/dev/null || echo '(no log file)'",
            ])
            .output()
            .await;

        match ssh_output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            Ok(out) => format!("SSH failed: {}", String::from_utf8_lossy(&out.stderr)),
            Err(e) => format!("SSH error: {}", e),
        }
    }

    /// Send a user message into the agent loop and collect all responses.
    pub async fn send_user_input(&self, text: &str, timeout: Duration) -> Vec<AgentResponse> {
        self.send_user_input_with_id(text, 1, timeout).await
    }

    /// Send a user message with a specific message_id and collect responses.
    pub async fn send_user_input_with_id(
        &self,
        text: &str,
        _msg_id: i32,
        timeout: Duration,
    ) -> Vec<AgentResponse> {
        self.send_message(text).await;
        self.collect_all_responses(timeout).await
    }

    /// Send a message WITHOUT waiting for responses (for concurrent testing).
    pub async fn send_message(&self, text: &str) {
        let message = Message::User {
            content: UserContent::Text(text.into()),
        };
        let origin = EntryOrigin::User {
            channel: "test".into(),
        };

        self.input_port
            .send_user_message(message, origin)
            .await
            .expect("input port should be open");
    }

    /// Collect all responses that arrive within the timeout.
    pub async fn collect_all_responses(&self, timeout: Duration) -> Vec<AgentResponse> {
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.entry_rx.lock().await;

        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(notification)) => {
                    if let Message::Assistant { content, .. } = &notification.entry.message {
                        let text = content.clone().unwrap_or_default();
                        let is_final = notification.is_final;
                        responses.push(AgentResponse {
                            text,
                            entry_id: notification.entry.id,
                            is_final,
                        });
                        if is_final {
                            // After an is_final response, keep waiting up to the
                            // original deadline for follow-up messages.
                            loop {
                                match tokio::time::timeout_at(deadline, rx.recv()).await {
                                    Ok(Ok(follow_up)) => {
                                        if let Message::Assistant { content, .. } =
                                            &follow_up.entry.message
                                        {
                                            let text = content.clone().unwrap_or_default();
                                            responses.push(AgentResponse {
                                                text,
                                                entry_id: follow_up.entry.id,
                                                is_final: follow_up.is_final,
                                            });
                                        }
                                    }
                                    _ => return responses,
                                }
                            }
                        }
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        responses
    }

    /// Collect responses for a specific msg_id.
    pub async fn collect_responses_for(
        &self,
        _msg_id: i32,
        timeout: Duration,
    ) -> Vec<AgentResponse> {
        // In the new architecture, we don't track message IDs at this level.
        // Just collect all responses.
        self.collect_all_responses(timeout).await
    }
}

impl Drop for VmSystemTestHarness {
    fn drop(&mut self) {
        // Abort the host task so it doesn't hang forever
        self.host_task.abort();
        // Clean up any leaked VMs
        cleanup_stale_vms();
        // _temp_dir will be dropped after this, cleaning up the directory
    }
}
