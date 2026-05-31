use std::path::PathBuf;
use std::process::Command;
use tokio::fs;

const BUILDS_DIR: &str = "target/release/.builds";
const CURRENT_FILE: &str = "target/release/.builds/current";
const PREVIOUS_FILE: &str = "target/release/.builds/previous";
const SYMLINK_PATH: &str = "target/release/rubberduxd";

pub async fn build_daemon() -> Result<String, String> {
    let sha = git_short_sha()?;

    let build_dir = PathBuf::from(BUILDS_DIR).join(&sha);
    let binary_path = build_dir.join("rubberduxd");

    // 1. Check if already built
    if binary_path.exists() {
        println!("Build already exists for {}. Skipping build.", sha);
        update_symlink(&sha).await?;
        return Ok(sha);
    }

    // 2. Build
    println!("Building rubberdux...");
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .status()
        .map_err(|e| format!("cargo build failed: {}", e))?;
    if !status.success() {
        return Err("cargo build --release failed".into());
    }

    // 3. Copy to versioned directory
    fs::create_dir_all(&build_dir)
        .await
        .map_err(|e| format!("Failed to create build dir: {}", e))?;

    let src = PathBuf::from("target/release/rubberduxd");
    fs::copy(&src, &binary_path)
        .await
        .map_err(|e| format!("Failed to copy binary: {}", e))?;

    // 4. Update current/previous tracking
    update_version_tracking(&sha).await?;

    // 5. Update symlink
    update_symlink(&sha).await?;

    println!("Build succeeded: {}", sha);
    Ok(sha)
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

async fn update_version_tracking(sha: &str) -> Result<(), String> {
    let current = fs::read_to_string(CURRENT_FILE).await.unwrap_or_default();
    let current = current.trim();

    if !current.is_empty() && current != sha {
        fs::write(PREVIOUS_FILE, current)
            .await
            .map_err(|e| format!("Failed to write previous: {}", e))?;
    }

    fs::write(CURRENT_FILE, sha)
        .await
        .map_err(|e| format!("Failed to write current: {}", e))?;

    Ok(())
}

pub async fn update_symlink(sha: &str) -> Result<(), String> {
    let target = format!(".builds/{}/rubberduxd", sha);
    let link = PathBuf::from(SYMLINK_PATH);

    // Remove existing file or symlink
    if link.exists() || is_symlink(&link) {
        fs::remove_file(&link)
            .await
            .map_err(|e| format!("Failed to remove old symlink/file: {}", e))?;
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &link)
            .map_err(|e| format!("Failed to create symlink: {}", e))?;
    }
    #[cfg(not(unix))]
    {
        let src = PathBuf::from(BUILDS_DIR).join(sha).join("rubberduxd");
        fs::copy(&src, &link)
            .await
            .map_err(|e| format!("Failed to copy binary: {}", e))?;
    }

    Ok(())
}

fn is_symlink(path: &PathBuf) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

pub async fn current_sha() -> Option<String> {
    fs::read_to_string(CURRENT_FILE)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn previous_sha() -> Option<String> {
    fs::read_to_string(PREVIOUS_FILE)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn get_binary_path() -> Result<PathBuf, String> {
    let sha = current_sha().await.ok_or("No current build found")?;
    let path = PathBuf::from(BUILDS_DIR).join(&sha).join("rubberduxd");
    if path.exists() {
        Ok(path)
    } else {
        Err(format!("Binary not found: {}", path.display()))
    }
}
