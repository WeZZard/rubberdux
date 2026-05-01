pub mod attribution;
pub mod cel_eval;
pub mod discovery;
pub mod evaluator;
pub mod execution;
pub mod format;
pub mod guidance;
pub mod lines;
pub mod linter;
pub mod llm;
pub mod narration;
pub mod ordering;
pub mod parser;
pub mod results;

pub use discovery::discover_cases;
pub use evaluator::{AssertionEvaluator, Evaluatable, EvaluationResult};
pub use execution::{
    AssistantSlotArtifact, ExchangeFailure, ExecutionArtifact, SlotArtifact,
    write_json_atomically, write_text_atomically,
};
pub use format::render_agent_input;
#[allow(deprecated)]
pub use lines::{
    AssertionLine, find_assistant_heading_lines, find_front_matter_key_line, find_heading_line,
    find_slot_heading_lines, find_user_heading_lines, map_assertion_lines,
};
pub use linter::{LintError, lint};
pub use llm::{ChatMessage, LlmClient, LlmError};
pub use ordering::{ActualMessage, MatchError, match_assistant_slots, match_slots};
pub use parser::{
    Assertion, FrontMatter, Message, OrderingDirective, SlotKind, TestCase, UserContent,
};
pub use results::{
    AssertionResult, AssertionScope, AttributionConfidence, FailureAttribution, TestResults, Vote,
    VoteDistribution,
};
