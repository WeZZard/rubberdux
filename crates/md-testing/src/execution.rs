use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::OrderingDirective;

/// Immutable record of a testcase execution before LLM-based evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionArtifact {
    pub testcase_name: String,
    pub run_id: String,
    pub timestamp: String,
    pub target: String,
    pub case_content: String,
    pub trajectory_markdown: String,
    pub user_messages: Vec<String>,
    pub assistant_slots: Vec<AssistantSlotArtifact>,
    pub actual_assistant_count: usize,
    pub exchange_failures: Vec<ExchangeFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantSlotArtifact {
    pub directive: OrderingDirective,
    pub assertions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeFailure {
    pub user_message_index: usize,
    pub reason: String,
}

impl ExecutionArtifact {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        write_json_atomically(path, self)
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

pub fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;

    let temp_path = temp_path_for(path);

    let result = (|| {
        let json = serde_json::to_vec_pretty(value)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    result
}

pub fn write_text_atomically(path: &Path, content: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;

    let temp_path = temp_path_for(path);

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    result
}

fn temp_path_for(path: &Path) -> std::path::PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "artifact.json".into());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_file_name(format!(
        "{}.tmp.{}.{}",
        file_name,
        std::process::id(),
        nanos
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OrderingDirective;

    #[test]
    fn execution_artifact_round_trips_through_atomic_write() {
        let dir = std::env::temp_dir().join(format!(
            "md-testing-execution-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("execution.json");

        let artifact = ExecutionArtifact {
            testcase_name: "new_agent_loop_u2a_1t_greeting".to_string(),
            run_id: "run".to_string(),
            timestamp: "2026-04-26-00-00-00-UTC".to_string(),
            target: "agent-loop".to_string(),
            case_content: "## Storyline\n<!-- ok -->".to_string(),
            trajectory_markdown: "# transcript".to_string(),
            user_messages: vec!["hello".to_string()],
            assistant_slots: vec![AssistantSlotArtifact {
                directive: OrderingDirective::Check,
                assertions: vec!["responds".to_string()],
            }],
            actual_assistant_count: 1,
            exchange_failures: vec![ExchangeFailure {
                user_message_index: 0,
                reason: "none".to_string(),
            }],
        };

        artifact.write(&path).unwrap();

        assert_eq!(ExecutionArtifact::read(&path).unwrap(), artifact);
        assert!(!dir.read_dir().unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));

        fs::remove_dir_all(&dir).unwrap();
    }
}
