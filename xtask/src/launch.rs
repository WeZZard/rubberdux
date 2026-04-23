use std::path::PathBuf;
use std::process::{Command, Stdio};
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
    let symlink_exists = std::fs::symlink_metadata(&sessions_link)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !symlink_exists && !sessions_link.exists() {
        let home = PathBuf::from(std::env::var("RUBBERDUX_HOME").unwrap());
        let target = home.join("sessions");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &sessions_link)
                .map_err(|e| format!("Failed to create sessions symlink: {}", e))?;
            println!("Created sessions symlink: {} -> {}", sessions_link.display(), target.display());
        }
    }

    let session_dir = project_dir.join("sessions");

    // Resolve symlink if sessions is a symlink, otherwise use as-is
    let session_dir = if let Ok(metadata) = std::fs::symlink_metadata(&session_dir) {
        if metadata.file_type().is_symlink() {
            std::fs::read_link(&session_dir).unwrap_or(session_dir)
        } else {
            session_dir
        }
    } else {
        session_dir
    };

    let log_file = session_dir.join("launch.log");
    let pid_file = session_dir.join("rubberdux.pid");

    fs::create_dir_all(&session_dir).await
        .map_err(|e| format!("Failed to create session directory: {}", e))?;

    // Provision VMs
    println!("Provisioning VMs...");
    provision_images(None).await?;

    // Stop existing instance
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

    // Archive previous session
    let session_jsonl = session_dir.join("session.jsonl");
    if session_jsonl.exists() {
        let now = time::OffsetDateTime::now_utc();
        let timestamp = format!("{:04}{:02}{:02}_{:02}{:02}{:02}", now.year(), now.month() as u8, now.day(), now.hour(), now.minute(), now.second());
        let archive_name = format!("session.{}.jsonl", timestamp);
        let archive_path = session_dir.join(&archive_name);
        fs::rename(&session_jsonl, &archive_path).await
            .map_err(|e| format!("Failed to archive session: {}", e))?;
        println!("Archived previous session to {}", archive_name);
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

    // Launch as background process with log file
    println!("Launching rubberdux (log: {})...", log_file.display());
    let log_file_std = std::fs::File::create(&log_file)
        .map_err(|e| format!("Failed to create log file: {}", e))?;
    let log_file_stderr = log_file_std.try_clone()
        .map_err(|e| format!("Failed to clone log file handle: {}", e))?;

    let child = Command::new("nohup")
        .args([
            "cargo", "run", "--release", "--", "--host"
        ])
        .stdout(Stdio::from(log_file_std))
        .stderr(Stdio::from(log_file_stderr))
        .spawn()
        .map_err(|e| format!("Failed to launch rubberdux: {}", e))?;

    let pid = child.id();
    fs::write(&pid_file, pid.to_string()).await
        .map_err(|e| format!("Failed to write PID file: {}", e))?;
    println!("rubberdux started (PID {})", pid);

    // Wait briefly for startup
    sleep(Duration::from_secs(2)).await;

    // Check if process is still running and show startup log
    let still_running = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if still_running {
        println!("--- Startup log (first 50 lines) ---");
        let log_content = fs::read_to_string(&log_file).await.unwrap_or_default();
        let lines: Vec<&str> = log_content.lines().collect();
        for line in lines.iter().take(50) {
            println!("{}", line);
        }
        println!("--- End startup log ---");
        println!("");
        println!("rubberdux is running. PID: {}", pid);
        println!("Full log: {}", log_file.display());
        println!("Stop with: cargo xtask stop");
    } else {
        println!("rubberdux exited immediately. Full log:");
        let log_content = fs::read_to_string(&log_file).await.unwrap_or_default();
        print!("{}", log_content);
        return Err("rubberdux exited immediately".into());
    }

    Ok(())
}
