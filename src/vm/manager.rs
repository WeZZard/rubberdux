use std::collections::HashMap;
use std::path::PathBuf;

use tokio::process::Command;

use crate::error::Error;

/// Handle for a running Tart VM.
struct VMHandle {
    /// Tart VM name (e.g., "rubberdux-main" or "rubberdux-abc123").
    name: String,
    /// Guest IP address (available after boot).
    ip: String,
}

/// Manages Tart VM lifecycles. All VMs are created on the host.
pub struct VMManager {
    default_image: String,
    share_root: PathBuf,
    active_vms: HashMap<String, VMHandle>,
}

impl VMManager {
    pub fn new(default_image: String, share_root: PathBuf) -> Self {
        Self {
            default_image,
            share_root,
            active_vms: HashMap::new(),
        }
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
    ) -> Result<String, Error> {
        let image = image.unwrap_or(&self.default_image);
        let name = format!("rubberdux-{}", vm_id);

        // Ensure share directory exists
        let share = self.share_dir(vm_id);
        tokio::fs::create_dir_all(&share).await?;

        // Clone base image
        run_tart(&["clone", image, &name]).await?;

        // Start the VM in the background with a shared directory
        let share_str = format!("share:{}", share.display());
        let mut child = Command::new("tart")
            .args(["run", &name, "--no-graphics", "--dir", &share_str])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::Vm(format!("failed to start VM {}: {}", name, e)))?;

        // Detach the child process so it survives independently
        let _ = child.id();
        tokio::spawn(async move {
            let _ = child.wait().await;
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

        let output = Command::new("ssh")
            .args([
                "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null",
                "-o", "ConnectTimeout=10",
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

        for attempt in 0..60 {
            let result = Command::new("ssh")
                .args([
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "ConnectTimeout=5",
                    "-o", "BatchMode=yes",
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
    for attempt in 0..30 {
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
            _ => {}
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err(Error::Vm(format!(
        "VM {} did not get an IP after 60 seconds",
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
