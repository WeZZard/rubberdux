use std::path::PathBuf;
use std::process::Command;
use tokio::fs;
use tokio::time::{sleep, Duration};

use crate::provision::provision_images;

pub async fn launch_rubberdux() -> Result<(), String> {
    let project_dir = std::env::current_dir()
        .map_err(|e| format!("Failed to get current directory: {}", e))?;

    // Read .env file if present
    let env_path = project_dir.join(".env");
    if env_path.exists() {
        println!("Loading environment from .env...");
        let _ = dotenvy::from_path(&env_path);
    }

    // Set RUBBERDUX_HOME if not set
    if std::env::var("RUBBERDUX_HOME").is_err() {
        let home = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".rubberdux");
        std::env::set_var("RUBBERDUX_HOME", &home);
        println!("Set RUBBERDUX_HOME to {}", home.display());
    }

    // Create project root symlink if missing
    let sessions_link = project_dir.join("sessions");
    if !sessions_link.exists() {
        let home = PathBuf::from(std::env::var("RUBBERDUX_HOME").unwrap());
        let target = home.join("sessions");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &sessions_link)
                .map_err(|e| format!("Failed to create sessions symlink: {}", e))?;
            println!("Created sessions symlink: {} -> {}", sessions_link.display(), target.display());
        }
    }

    // Provision VMs
    println!("Provisioning VMs...");
    provision_images(None).await?;

    // Stop existing instance
    let pid_file = project_dir.join("sessions").join("rubberdux.pid");
    if pid_file.exists() {
        let old_pid = fs::read_to_string(&pid_file).await.unwrap_or_default();
        let old_pid = old_pid.trim();
        if !old_pid.is_empty() {
            if let Ok(pid) = old_pid.parse::<i32>() {
                println!("Stopping existing instance (PID {})...", pid);
                let _ = Command::new("kill")
                    .arg(old_pid)
                    .status();
                sleep(Duration::from_secs(1)).await;
                let _ = Command::new("kill")
                    .args(["-9", old_pid])
                    .status();
            }
        }
        let _ = fs::remove_file(&pid_file).await;
    }

    // Stop leaked VMs
    let vm_list = Command::new("tart")
        .args(["list"])
        .output()
        .ok();
    if let Some(output) = vm_list {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.starts_with("local") && line.contains("rubberdux-") && line.contains("running") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let vm_name = parts[1];
                    println!("Stopping leaked VM: {}", vm_name);
                    let _ = Command::new("tart").args(["stop", vm_name]).status();
                    sleep(Duration::from_secs(1)).await;
                    let _ = Command::new("tart").args(["delete", vm_name]).status();
                }
            }
        }
    }

    // Build
    println!("Building rubberdux...");
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .status()
        .map_err(|e| format!("cargo build failed: {}", e))?;
    if !status.success() {
        return Err("cargo build --release failed".into());
    }
    println!("Build succeeded.");

    // Launch as background process
    println!("Launching rubberdux...");
    let child = Command::new("nohup")
        .args([
            "cargo", "run", "--release", "--", "--host"
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch rubberdux: {}", e))?;

    let pid = child.id();
    
    // Write PID file to project root for easy access
    let root_pid_file = project_dir.join("rubberdux.pid");
    fs::write(&root_pid_file, pid.to_string()).await
        .map_err(|e| format!("Failed to write PID file: {}", e))?;
    
    println!("rubberdux started (PID {})", pid);

    // Wait briefly for startup
    sleep(Duration::from_secs(2)).await;

    // Check if process is still running
    let still_running = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if still_running {
        println!("rubberdux is running. PID: {}", pid);
        println!("Session will be created in $RUBBERDUX_HOME/sessions/");
        println!("Latest session symlink: $RUBBERDUX_HOME/latest");
        println!("Stop with: cargo xtask stop");
    } else {
        println!("rubberdux exited immediately. Check logs in $RUBBERDUX_HOME/sessions/");
        return Err("rubberdux exited immediately".into());
    }

    Ok(())
}