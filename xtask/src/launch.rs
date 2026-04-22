use std::path::PathBuf;
use std::process::Command;
use tokio::fs;
use tokio::time::{sleep, Duration};

use crate::provision::provision_images;

fn current_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let tm = time::OffsetDateTime::from_unix_timestamp(secs as i64).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}{:02}{:02}_{:02}{:02}{:02}",
        tm.year(), tm.month() as u8, tm.day(),
        tm.hour(), tm.minute(), tm.second())
}

pub async fn launch_rubberdux() -> Result<(), String> {
    let project_dir = std::env::current_dir()
        .map_err(|e| format!("Failed to get current directory: {}", e))?;

    // Read .env file if present
    let env_path = project_dir.join(".env");
    if env_path.exists() {
        println!("Loading environment from .env...");
        let _ = dotenvy::from_path(&env_path);
    }

    // Provision VMs
    println!("Provisioning VMs...");
    provision_images(None).await?;

    let session_dir = PathBuf::from(
        std::env::var("RUBBERDUX_SESSION_DIR").unwrap_or_else(|_| "./sessions".into()));
    let log_file = session_dir.join("launch.log");
    let pid_file = session_dir.join("rubberdux.pid");

    fs::create_dir_all(&session_dir).await
        .map_err(|e| format!("Failed to create session dir: {}", e))?;

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
    let session_file = session_dir.join("main-agent.jsonl");
    if session_file.exists() {
        let archive_dir = session_dir.join("archive");
        fs::create_dir_all(&archive_dir).await
            .map_err(|e| format!("Failed to create archive dir: {}", e))?;
        let archive_name = format!(
            "main-agent.{}.jsonl",
            current_timestamp()
        );
        let archive_path = archive_dir.join(&archive_name);
        fs::rename(&session_file, &archive_path).await
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

    // Launch as background process
    println!("Launching rubberdux (log: {})...", log_file.display());
    let child = Command::new("nohup")
        .args([
            "cargo", "run", "--release", "--", "--host"
        ])
        .stdout(std::fs::File::create(&log_file).map_err(|e| format!("Failed to create log file: {}", e))?)
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch rubberdux: {}", e))?;

    let pid = child.id();
    fs::write(&pid_file, pid.to_string()).await
        .map_err(|e| format!("Failed to write PID file: {}", e))?;
    println!("rubberdux started (PID {})", pid);

    // Wait briefly for startup, then tail initial log
    sleep(Duration::from_secs(2)).await;

    // Check if process is still running
    let still_running = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if still_running {
        println!("--- Startup log (first 50 lines) ---");
        let log_content = fs::read_to_string(&log_file).await.unwrap_or_default();
        for line in log_content.lines().take(50) {
            println!("{}", line);
        }
        println!("--- End startup log ---");
        println!();
        println!("rubberdux is running. PID: {}", pid);
        println!("Full log: {}", log_file.display());
        println!("Stop with: kill {}", pid);
    } else {
        println!("rubberdux exited immediately. Full log:");
        let log_content = fs::read_to_string(&log_file).await.unwrap_or_default();
        println!("{}", log_content);
        return Err("rubberdux exited immediately".into());
    }

    Ok(())
}
