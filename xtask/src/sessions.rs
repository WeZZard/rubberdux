use std::fs;
use std::path::PathBuf;

fn rubberdux_home() -> PathBuf {
    std::env::var("RUBBERDUX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".rubberdux")
        })
}

fn sessions_dir() -> PathBuf {
    rubberdux_home().join("sessions")
}

fn latest_link() -> PathBuf {
    rubberdux_home().join("latest")
}

pub fn list_sessions() {
    let sessions_dir = sessions_dir();

    if !sessions_dir.exists() {
        println!("No sessions directory found at {}", sessions_dir.display());
        return;
    }

    let latest = if latest_link().exists() {
        fs::read_link(latest_link()).ok()
    } else {
        None
    };

    let mut entries: Vec<_> = fs::read_dir(&sessions_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    entries.sort();

    if entries.is_empty() {
        println!("No sessions found.");
        return;
    }

    println!("Sessions:");
    for entry in entries {
        let is_latest = latest
            .as_ref()
            .map(|l| {
                l.file_name()
                    .map(|n| n.to_string_lossy() == entry)
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        let marker = if is_latest { " (latest)" } else { "" };
        let session_dir = sessions_dir.join(&entry);

        // Count subagents
        let subagent_count = fs::read_dir(&session_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("agent_")
                    && e.file_type().map(|t| t.is_dir()).unwrap_or(false)
            })
            .count();

        println!(
            "  {}{} - agent_main + {} subagent(s)",
            entry, marker, subagent_count
        );
    }
}

pub fn archive_session(session_id: &str) {
    let session_dir = sessions_dir().join(session_id);

    if !session_dir.exists() {
        eprintln!("Session '{}' not found.", session_id);
        std::process::exit(1);
    }

    let archive_dir = rubberdux_home().join("archive").join(session_id);
    fs::create_dir_all(archive_dir.parent().unwrap()).unwrap();

    if let Err(e) = fs::rename(&session_dir, &archive_dir) {
        eprintln!("Failed to archive session: {}", e);
        std::process::exit(1);
    }

    println!(
        "Archived session '{}' to {}",
        session_id,
        archive_dir.display()
    );
}

pub fn delete_session(session_id: &str) {
    let session_dir = sessions_dir().join(session_id);

    if !session_dir.exists() {
        eprintln!("Session '{}' not found.", session_id);
        std::process::exit(1);
    }

    if let Err(e) = fs::remove_dir_all(&session_dir) {
        eprintln!("Failed to delete session: {}", e);
        std::process::exit(1);
    }

    println!("Deleted session '{}'", session_id);
}

pub fn clear_sessions() {
    let sessions_dir = sessions_dir();

    if !sessions_dir.exists() {
        println!("No sessions to clear.");
        return;
    }

    // Get latest session to preserve it
    let latest = if latest_link().exists() {
        fs::read_link(latest_link())
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
    } else {
        None
    };

    let entries: Vec<_> = fs::read_dir(&sessions_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let mut removed = 0;
    for entry in entries {
        if Some(&entry) == latest.as_ref() {
            println!("Preserving latest session: {}", entry);
            continue;
        }

        let path = sessions_dir.join(&entry);
        if let Err(e) = fs::remove_dir_all(&path) {
            eprintln!("Failed to remove {}: {}", entry, e);
        } else {
            removed += 1;
        }
    }

    println!("Cleared {} session(s).", removed);
}
