use std::process::Command;

pub async fn stop_rubberdux() -> Result<(), String> {
    let project_dir = std::env::current_dir()
        .map_err(|e| format!("Failed to get current directory: {}", e))?;

    // Read PID file
    let pid_file = project_dir.join("rubberdux.pid");
    if pid_file.exists() {
        let pid_str = tokio::fs::read_to_string(&pid_file).await
            .map_err(|e| format!("Failed to read PID file: {}", e))?;
        let pid = pid_str.trim();
        if !pid.is_empty() {
            if let Ok(pid_num) = pid.parse::<i32>() {
                println!("Stopping rubberdux (PID {})...", pid_num);
                let _ = Command::new("kill").arg(pid).status();
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                let _ = Command::new("kill").args(["-9", pid]).status();
            }
        }
        let _ = tokio::fs::remove_file(&pid_file).await;
        println!("Stopped rubberdux.");
    } else {
        println!("No PID file found.");
    }

    // Stop any running Tart VMs
    let vm_count = stop_leaked_vms().await;
    if vm_count > 0 {
        println!("Stopped {} VM(s).", vm_count);
    }

    Ok(())
}

async fn stop_leaked_vms() -> usize {
    let mut count = 0;
    let output = Command::new("tart")
        .args(["list"])
        .output();
    
    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.starts_with("local") && line.contains("rubberdux-") && line.contains("running") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let vm_name = parts[1];
                    println!("Stopping VM: {}", vm_name);
                    let _ = Command::new("tart").args(["stop", vm_name]).status();
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    let _ = Command::new("tart").args(["delete", vm_name]).status();
                    count += 1;
                }
            }
        }
    }
    
    count
}
