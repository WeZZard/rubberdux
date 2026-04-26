use serde::{Deserialize, Serialize};
use std::path::Path;

/// Machine-readable test results for a single test case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResults {
    pub testcase_name: String,
    pub run_id: String,
    pub timestamp: String,
    pub target: String,
    pub passed: bool,
    pub assertions: Vec<AssertionResult>,
}

/// A single assertion evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    pub scope: AssertionScope,
    pub line: usize,
    pub assertion: String,
    pub passed: bool,
    pub reasoning: String,
}

/// The scope of an evaluation assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssertionScope {
    Storyline,
    UserMessage {
        msg_index: usize,
    },
    AssistantMessage {
        slot_index: usize,
        actual_index: Option<usize>,
    },
    OrderingMatch,
}

impl TestResults {
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        crate::execution::write_json_atomically(path, self)
    }

    pub fn read(path: &Path) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let results = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(results)
    }
}
