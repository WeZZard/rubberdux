use super::ToolResult;

pub async fn execute(args: &serde_json::Value) -> ToolResult {
    let command = match args["command"].as_str() {
        Some(c) => c,
        None => {
            return ToolResult {
                content: "Missing required parameter: command".into(),
                is_error: true,
            }
        }
    };

    let run_in_bg = args["run_in_background"].as_bool().unwrap_or(false);
    let timeout_ms = args["timeout"].as_u64().unwrap_or(120_000);

    if run_in_bg {
        execute_background(command).await
    } else {
        execute_sync(command, timeout_ms).await
    }
}

async fn execute_sync(command: &str, timeout_ms: u64) -> ToolResult {
    let timeout = std::time::Duration::from_millis(timeout_ms);

    let result = tokio::time::timeout(
        timeout,
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut content = String::new();
            if !stdout.is_empty() {
                content.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str("STDERR: ");
                content.push_str(&stderr);
            }
            if content.is_empty() {
                content = "(no output)".into();
            }

            ToolResult {
                content,
                is_error: !output.status.success(),
            }
        }
        Ok(Err(e)) => ToolResult {
            content: format!("Failed to execute command: {}", e),
            is_error: true,
        },
        Err(_) => ToolResult {
            content: format!("Command timed out after {}ms", timeout_ms),
            is_error: true,
        },
    }
}

async fn execute_background(command: &str) -> ToolResult {
    let task_id = format!("bg_{}", generate_task_id());
    let output_dir = std::path::PathBuf::from("./sessions/tasks");
    let _ = std::fs::create_dir_all(&output_dir);

    let output_path = output_dir.join(format!("{}.output", task_id));
    let output_path_str = output_path.to_string_lossy().to_string();

    let command = command.to_owned();
    let path = output_path.clone();

    tokio::spawn(async move {
        let result = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await;

        let content = match result {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    text.push_str("\nSTDERR: ");
                    text.push_str(&stderr);
                }
                if !output.status.success() {
                    text.push_str(&format!("\nExit code: {}", output.status));
                }
                text
            }
            Err(e) => format!("Failed to execute command: {}", e),
        };

        if let Err(e) = std::fs::write(&path, &content) {
            log::error!("Failed to write background task output: {}", e);
        }

        log::info!("Background task {} completed", path.display());
    });

    ToolResult {
        content: format!(
            "Command running in background with ID: {}. Output is being written to: {}",
            task_id, output_path_str
        ),
        is_error: false,
    }
}

fn generate_task_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:x}", ts & 0xFFFF_FFFF)
}
