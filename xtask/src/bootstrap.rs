use std::process::Command;
use tokio::time::{sleep, Duration};

use crate::build::{build_daemon, current_sha, previous_sha, update_symlink};
use crate::launch::launch_rubberdux;

pub async fn bootstrap() -> Result<(), String> {
    let sha = git_short_sha()?;

    // 1. Build if needed
    let current = current_sha().await;
    if current.as_ref() != Some(&sha) {
        build_daemon().await?;
    } else {
        println!("Build already up to date: {}", sha);
    }

    // 2. Launch if not running or running old version
    if !host_alive() || current.as_ref() != Some(&sha) {
        launch_rubberdux().await?;
    } else {
        println!("Host already running with current version.");
    }

    // 3. Health check
    println!("Waiting for health check...");
    sleep(Duration::from_secs(5)).await;

    if host_alive() {
        println!("Bootstrap succeeded.");
        return Ok(());
    }

    // 4. Rollback
    println!("Health check failed.");
    if let Some(prev) = previous_sha().await {
        println!("Rolling back to {}...", prev);
        update_symlink(&prev).await?;
        launch_rubberdux().await?;

        sleep(Duration::from_secs(5)).await;
        if host_alive() {
            println!("Rollback succeeded.");
            return Ok(());
        } else {
            return Err("Rollback failed: host still unhealthy.".into());
        }
    }

    Err("Health check failed and no previous version available.".into())
}

fn git_short_sha() -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map_err(|e| format!("git rev-parse failed: {}", e))?;
    if !output.status.success() {
        return Err("git rev-parse failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string())
}

pub fn host_alive() -> bool {
    let output = Command::new("pgrep")
        .args(["-f", "rubberduxd --host"])
        .output();

    match output {
        Ok(output) => !output.stdout.is_empty(),
        Err(_) => false,
    }
}
