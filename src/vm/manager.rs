use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;

use crate::error::Error;
use crate::vm::setup::ssh_private_key;

static VM_NAME_COUNTER: AtomicU64 = AtomicU64::new(1);

fn generate_vm_name(vm_id: &str) -> String {
    let counter = VM_NAME_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("rubberdux-{}-{}", vm_id, counter)
}

/// Handle for a running Tart VM.
struct VMHandle {
    /// Tart VM name (e.g., "rubberdux-main-1").
    name: String,
    /// Guest IP address (available after boot).
    ip: String,
}

/// Manages Tart VM lifecycles. All VMs are created on the host.
pub struct VMManager {
    default_image: String,
    share_root: PathBuf,
    memory_mb: Option<usize>,
    cpu_count: Option<usize>,
    active_vms: HashMap<String, VMHandle>,
}

impl VMManager {
    pub fn new(default_image: String, share_root: PathBuf) -> Self {
        Self {
            default_image,
            share_root,
            memory_mb: None,
            cpu_count: None,
            active_vms: HashMap::new(),
        }
    }

    pub fn with_memory_mb(mut self, memory_mb: usize) -> Self {
        self.memory_mb = Some(memory_mb);
        self
    }

    pub fn with_cpu_count(mut self, cpu_count: usize) -> Self {
        self.cpu_count = Some(cpu_count);
        self
    }

    pub fn default_image(&self) -> &str {
        &self.default_image
    }

    /// Shared directory on the host for a given VM.
    pub fn share_dir(&self, vm_id: &str) -> PathBuf {
        self.share_root.join(vm_id)
    }

    /// Clone a base image and start a VM. Returns the guest IP.
    pub async fn create_and_start(
        &mut self,
        vm_id: &str,
        image: Option<&str>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<String, Error> {
        let image = image.unwrap_or(&self.default_image);
        let name = generate_vm_name(vm_id);

        // Ensure share directory exists
        let share = self.share_dir(vm_id);
        tokio::fs::create_dir_all(&share).await?;

        // Clone base image
        run_tart(&["clone", image, &name]).await?;

        // Optionally restrict memory and CPU so multiple VMs can run simultaneously
        if let Some(mem) = self.memory_mb {
            let _ = run_tart(&["set", &name, "--memory", &mem.to_string()]).await;
        }
        if let Some(cpus) = self.cpu_count {
            let _ = run_tart(&["set", &name, "--cpu", &cpus.to_string()]).await;
        }

        // Start the VM in the background with shared directories
        let share_str = format!("share:{}", share.display());
        let mut tart_args = vec![
            "run".to_string(),
            name.clone(),
            "--no-graphics".to_string(),
            "--dir".to_string(),
            share_str,
        ];
        if let Some(data) = data_dir {
            tart_args.push("--dir".to_string());
            tart_args.push(format!("data:{}", data.display()));
        }
        let mut child = Command::new("tart")
            .args(&tart_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Vm(format!("failed to start VM {}: {}", name, e)))?;

        // Detach the child process so it survives independently
        let _ = child.id();
        let vm_name_for_log = name.clone();
        tokio::spawn(async move {
            let output = child.wait_with_output().await;
            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !out.status.success() {
                        log::warn!(
                            "[tart run {}] exited with status {} stdout={} stderr={}",
                            vm_name_for_log,
                            out.status,
                            stdout,
                            stderr
                        );
                    }
                }
                Err(e) => {
                    log::warn!("[tart run {}] wait failed: {}", vm_name_for_log, e);
                }
            }
        });

        // Wait for the VM to get an IP (poll with backoff)
        let ip = wait_for_ip(&name).await?;

        self.active_vms.insert(
            vm_id.to_string(),
            VMHandle {
                name,
                ip: ip.clone(),
            },
        );

        Ok(ip)
    }

    /// Get the IP of a running VM.
    pub fn ip(&self, vm_id: &str) -> Option<&str> {
        self.active_vms.get(vm_id).map(|h| h.ip.as_str())
    }

    /// Execute a command inside the VM via SSH.
    pub async fn exec(&self, vm_id: &str, command: &str) -> Result<ExecResult, Error> {
        let handle = self
            .active_vms
            .get(vm_id)
            .ok_or_else(|| Error::Vm(format!("no VM with id {}", vm_id)))?;

        let key_path = ssh_private_key();
        let output = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ConnectTimeout=10",
                "-i", &key_path.to_string_lossy(),
                &format!("admin@{}", handle.ip),
                command,
            ])
            .output()
            .await
            .map_err(|e| Error::Vm(format!("ssh exec failed: {}", e)))?;

        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    /// Wait for SSH to become available on a VM.
    pub async fn wait_for_ssh(&self, vm_id: &str) -> Result<(), Error> {
        let handle = self
            .active_vms
            .get(vm_id)
            .ok_or_else(|| Error::Vm(format!("no VM with id {}", vm_id)))?;

        let key_path = ssh_private_key();
        for attempt in 0..60 {
            let result = Command::new("ssh")
                .args([
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "ConnectTimeout=5",
                    "-o", "BatchMode=yes",
                    "-i", &key_path.to_string_lossy(),
                    &format!("admin@{}", handle.ip),
                    "true",
                ])
                .output()
                .await;

            if let Ok(output) = result {
                if output.status.success() {
                    log::info!("VM {} SSH ready after {} attempts", vm_id, attempt + 1);
                    return Ok(());
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        Err(Error::Vm(format!(
            "VM {} SSH not ready after 120 seconds",
            vm_id
        )))
    }

    /// Copy the agent binary to the VM via SCP.
    pub async fn copy_agent_binary(&self, vm_id: &str) -> Result<(), Error> {
        let handle = self
            .active_vms
            .get(vm_id)
            .ok_or_else(|| Error::Vm(format!("no VM with id {}", vm_id)))?;

        let binary_path = if cfg!(target_os = "macos") {
            std::env::current_dir()
                .unwrap_or_default()
                .join("target")
                .join("aarch64-unknown-linux-musl")
                .join("release")
                .join("rubberdux")
        } else {
            std::env::current_exe()
                .map_err(|e| Error::Vm(format!("failed to get current exe: {}", e)))?
        };

        if !binary_path.exists() {
            return Err(Error::Vm(format!(
                "agent binary not found at {}",
                binary_path.display()
            )));
        }

        let key_path = ssh_private_key();
        let scp_result = Command::new("scp")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ConnectTimeout=10",
                "-i", &key_path.to_string_lossy(),
                &binary_path.to_string_lossy(),
                &format!("admin@{}:/tmp/rubberdux.new", handle.ip),
            ])
            .output()
            .await
            .map_err(|e| Error::Vm(format!("scp failed: {}", e)))?;

        if !scp_result.status.success() {
            let stderr = String::from_utf8_lossy(&scp_result.stderr);
            return Err(Error::Vm(format!("scp failed: {}", stderr)));
        }

        let ssh_result = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ConnectTimeout=10",
                "-i", &key_path.to_string_lossy(),
                &format!("admin@{}", handle.ip),
                "sudo cp /tmp/rubberdux.new /usr/local/bin/rubberdux && sudo chmod +x /usr/local/bin/rubberdux",
            ])
            .output()
            .await
            .map_err(|e| Error::Vm(format!("ssh install failed: {}", e)))?;

        if !ssh_result.status.success() {
            let stderr = String::from_utf8_lossy(&ssh_result.stderr);
            return Err(Error::Vm(format!("install failed: {}", stderr)));
        }

        log::info!("Copied agent binary to VM {}", vm_id);
        Ok(())
    }

    /// Stop and delete a VM.
    pub async fn destroy(&mut self, vm_id: &str) -> Result<(), Error> {
        let handle = match self.active_vms.remove(vm_id) {
            Some(h) => h,
            None => return Ok(()),
        };

        let _ = run_tart(&["stop", &handle.name]).await;
        let _ = run_tart(&["delete", &handle.name]).await;

        // Clean up share directory
        let share = self.share_dir(vm_id);
        let _ = tokio::fs::remove_dir_all(&share).await;

        log::info!("Destroyed VM {} ({})", vm_id, handle.name);
        Ok(())
    }

    /// Destroy all active VMs.
    pub async fn destroy_all(&mut self) {
        let ids: Vec<String> = self.active_vms.keys().cloned().collect();
        for id in ids {
            if let Err(e) = self.destroy(&id).await {
                log::warn!("Failed to destroy VM {}: {}", id, e);
            }
        }
    }
}

/// Result of executing a command inside a VM.
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn run_tart(args: &[&str]) -> Result<String, Error> {
    let output = Command::new("tart")
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Vm(format!("tart command failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Vm(format!(
            "tart {} failed: {}",
            args.first().unwrap_or(&""),
            stderr
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn wait_for_ip(vm_name: &str) -> Result<String, Error> {
    // Poll for up to 180 seconds to tolerate concurrent VM boot on busy hosts.
    for attempt in 0..90 {
        match run_tart(&["ip", vm_name]).await {
            Ok(ip) if !ip.is_empty() => {
                log::info!(
                    "VM {} got IP {} after {} attempts",
                    vm_name,
                    ip,
                    attempt + 1
                );
                return Ok(ip);
            }
            Ok(ip) => {
            }
            Err(e) => {
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(Error::Vm(format!(
        "VM {} did not get an IP after 180 seconds",
        vm_name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_share_dir() {
        let mgr = VMManager::new("img".into(), PathBuf::from("/tmp/shares"));
        assert_eq!(mgr.share_dir("main"), PathBuf::from("/tmp/shares/main"));
        assert_eq!(mgr.share_dir("task-1"), PathBuf::from("/tmp/shares/task-1"));
    }
}
