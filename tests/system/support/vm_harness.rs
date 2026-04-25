use std::path::{Path, PathBuf};
use std::time::Duration;

use rubberdux::channel::interpreter::InterpretedMessage;
use rubberdux::channel::{AgentResponse, ChannelEvent};
use rubberdux::host::{self, HostConfig};
use rubberdux::vm::setup::ssh_private_key;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::setup::{cleanup_stale_vms, linux_agent_binary_path};

pub struct VmSystemTestHarness {
    _temp_dir: tempfile::TempDir,
    telegram_tx: mpsc::Sender<ChannelEvent>,
    telegram_response_rx: tokio::sync::Mutex<mpsc::Receiver<AgentResponse>>,
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
        let host_config = HostConfig {
            vm_image: "rubberdux-base-ubuntu24-release".into(),
            share_root: share_root.clone(),
            rpc_port: 0,
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

        // 8. Channels for host
        let (telegram_tx, telegram_rx) = mpsc::channel::<ChannelEvent>(32);
        let (telegram_response_tx, telegram_response_rx) = mpsc::channel::<AgentResponse>(32);

        // 9. Spawn host
        let host_task = tokio::spawn(async move {
            host::run(host_config, telegram_rx, telegram_response_tx).await;
        });

        Self {
            _temp_dir: temp_dir,
            telegram_tx,
            telegram_response_rx: tokio::sync::Mutex::new(telegram_response_rx),
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
            .and(path("/chat/completions"))
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
    /// `host.rs` copies /tmp/rubberdux-agent.log into the share as agent.log
    /// before destroying the VM, so the log survives cleanup.
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

    /// Send a user message into the host and collect all AgentResponses from the
    /// host's output channel. Returns when the timeout expires or when no new
    /// responses arrive for a short grace period after an `is_final` response.
    pub async fn send_user_input(&self, text: &str, timeout: Duration) -> Vec<AgentResponse> {
        self.send_user_input_with_id(text, 1, timeout).await
    }

    /// Send a user message with a specific telegram_message_id and collect responses.
    pub async fn send_user_input_with_id(
        &self,
        text: &str,
        msg_id: i32,
        timeout: Duration,
    ) -> Vec<AgentResponse> {
        self.send_message_with_id(text, msg_id).await;
        self.collect_responses_for(msg_id, timeout).await
    }

    /// Send a message WITHOUT waiting for responses (for concurrent testing).
    pub async fn send_message_with_id(&self, text: &str, msg_id: i32) {
        let event = ChannelEvent::UserInput {
            interpreted: InterpretedMessage {
                text: text.into(),
                attachments: vec![],
            },
            reply_tx: None,
            telegram_message_id: Some(msg_id),
        };

        self.telegram_tx
            .send(event)
            .await
            .expect("telegram channel should be open");
    }

    /// Collect all responses that arrive within the timeout.
    pub async fn collect_all_responses(&self, timeout: Duration) -> Vec<AgentResponse> {
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.telegram_response_rx.lock().await;

        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(response)) => {
                    let is_final = response.is_final;
                    responses.push(response);
                    if is_final {
                        // After an is_final response, keep waiting up to the
                        // original deadline for follow-up messages (e.g. child
                        // VM results that arrive later).
                        loop {
                            match tokio::time::timeout_at(deadline, rx.recv()).await {
                                Ok(Some(follow_up)) => {
                                    responses.push(follow_up);
                                }
                                _ => return responses,
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        responses
    }

    /// Collect responses for a specific msg_id.
    /// Note: This collects ALL responses first, then filters. Use this after
    /// all messages have been sent.
    pub async fn collect_responses_for(
        &self,
        msg_id: i32,
        timeout: Duration,
    ) -> Vec<AgentResponse> {
        let all_responses = self.collect_all_responses(timeout).await;
        all_responses
            .into_iter()
            .filter(|r| r.reply_to_message_id == Some(msg_id))
            .collect()
    }
}

impl Drop for VmSystemTestHarness {
    fn drop(&mut self) {
        // Abort the host task so it doesn't hang forever on ctrl_c
        self.host_task.abort();
        // Clean up any leaked VMs
        cleanup_stale_vms();
        // _temp_dir will be dropped after this, cleaning up the directory
    }
}
