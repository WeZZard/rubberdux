use md_testing::{AssertionResult, AssertionScope, TestResults};
use tower_lsp::lsp_types::*;

/// Build diagnostic with icon + detailed hover info
pub fn build_icon_diagnostic(assertion: &AssertionResult, results: &TestResults) -> Diagnostic {
    let line = (assertion.line.saturating_sub(1)) as u32;
    let range = Range {
        start: Position { line, character: 0 },
        end: Position { line, character: 0 },
    };

    let severity = if assertion.passed {
        DiagnosticSeverity::INFORMATION
    } else {
        DiagnosticSeverity::ERROR
    };

    let status = if assertion.passed {
        "✓ Passed"
    } else {
        "✗ Failed"
    };
    let message = if assertion.passed {
        format!(
            "{}\n\nAssertion: {}\nRun: {}\nTarget: {}",
            status, assertion.assertion, results.run_id, results.target
        )
    } else {
        format!(
            "{}\n\nReasoning: {}\nRun: {}\nTarget: {}",
            status, assertion.reasoning, results.run_id, results.target
        )
    };

    let code = match &assertion.scope {
        AssertionScope::Storyline => Some("storyline".to_string()),
        AssertionScope::UserMessage { msg_index } => Some(format!("user-msg-{}", msg_index)),
        AssertionScope::AssistantMessage { slot_index, .. } => {
            Some(format!("assistant-msg-{}", slot_index))
        }
        AssertionScope::OrderingMatch => Some("ordering".to_string()),
    };

    Diagnostic {
        range,
        severity: Some(severity),
        code: code.map(NumberOrString::String),
        source: Some("md-testing".to_string()),
        message,
        ..Default::default()
    }
}
