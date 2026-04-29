use md_testing::{AssertionResult, AssertionScope, FailureAttribution, TestResults};
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

    let attribution_line = assertion.attribution.as_ref().map(format_attribution);

    let message = if assertion.passed {
        format!(
            "{}\n\nAssertion: {}\nRun: {}\nTarget: {}",
            status, assertion.assertion, results.run_id, results.target
        )
    } else {
        let mut msg = format!(
            "{}\n\nReasoning: {}\nRun: {}\nTarget: {}",
            status, assertion.reasoning, results.run_id, results.target
        );
        if let Some(attr) = &attribution_line {
            msg.push_str(&format!("\nAttribution: {}", attr));
        }
        msg
    };

    let code = match &assertion.scope {
        AssertionScope::FrontMatter { key } => Some(format!("front-matter-{}", key)),
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

fn format_attribution(attr: &FailureAttribution) -> String {
    match attr {
        FailureAttribution::SubjectFailure {
            confidence,
            evidence,
        } => format!("Subject Failure ({:?}) — {}", confidence, evidence),
        FailureAttribution::AssertionDefect {
            confidence,
            evidence,
        } => format!("Assertion Defect ({:?}) — {}", confidence, evidence),
        FailureAttribution::JudgePipelineFailure {
            confidence,
            evidence,
        } => format!("Judge Pipeline Failure ({:?}) — {}", confidence, evidence),
    }
}
