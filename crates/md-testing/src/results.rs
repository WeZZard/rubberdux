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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_call_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<FailureAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote_distribution: Option<VoteDistribution>,
}

/// The scope of an evaluation assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssertionScope {
    FrontMatter {
        key: String,
    },
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

/// Attribution of why an assertion failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "category", rename_all = "snake_case")]
pub enum FailureAttribution {
    SubjectFailure {
        confidence: AttributionConfidence,
        evidence: String,
    },
    AssertionDefect {
        confidence: AttributionConfidence,
        evidence: String,
    },
    JudgePipelineFailure {
        confidence: AttributionConfidence,
        evidence: String,
    },
}

/// How confident the attribution is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionConfidence {
    Definitive,
    High,
    Moderate,
    Low,
}

/// Per-vote breakdown from self-consistency evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteDistribution {
    pub total_votes: usize,
    pub pass_votes: usize,
    pub fail_votes: usize,
    pub error_votes: usize,
    pub votes: Vec<Vote>,
}

/// A single judge vote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    pub passed: bool,
    pub reasoning: String,
    pub duration_ms: u64,
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
