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
    AssistantSlotArtifact, ExchangeFailure, ExecutionArtifact, write_json_atomically,
    write_text_atomically,
};
pub use format::render_agent_input;
pub use lines::{
    AssertionLine, find_assistant_heading_lines, find_heading_line, find_user_heading_lines,
    map_assertion_lines,
};
pub use linter::{LintError, lint};
pub use llm::{ChatMessage, LlmClient, LlmError};
pub use ordering::{MatchError, match_assistant_slots};
pub use parser::{FrontMatter, Message, OrderingDirective, TestCase, UserContent};
pub use results::{AssertionResult, AssertionScope, TestResults};
