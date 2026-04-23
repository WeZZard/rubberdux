use std::path::Path;

use md_testing::{lint, AssertionScope, TestResults};
use tower_lsp::lsp_types::*;

use crate::results::ResultsStore;

/// Build LSP diagnostics for a testcase.md file.
pub async fn build_diagnostics(
    content: &str,
    uri: &Url,
    results_store: &ResultsStore,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // 1. Lint diagnostics (always shown)
    diagnostics.extend(build_lint_diagnostics(content));

    // 2. Test result diagnostics (if results exist)
    let case_name = extract_case_name(uri);
    if let Some(results) = results_store.get_results(&case_name).await {
        diagnostics.extend(build_result_diagnostics(content, &results));
    }

    diagnostics
}

fn build_lint_diagnostics(content: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    match lint(content) {
        Ok(()) => {}
        Err(errors) => {
            for error in errors {
                let range = Range {
                    start: Position {
                        line: (error.line.saturating_sub(1)) as u32,
                        character: 0,
                    },
                    end: Position {
                        line: (error.line.saturating_sub(1)) as u32,
                        character: 0,
                    },
                };

                diagnostics.push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: Some(NumberOrString::String(error.rule.to_string())),
                    source: Some("md-testing-lint".to_string()),
                    message: error.message,
                    ..Default::default()
                });
            }
        }
    }

    diagnostics
}

fn build_result_diagnostics(
    _content: &str,
    results: &TestResults,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for assertion in &results.assertions {
        let line = (assertion.line.saturating_sub(1)) as u32;
        let range = Range {
            start: Position { line, character: 0 },
            end: Position { line, character: 0 },
        };

        let (severity, prefix) = if assertion.passed {
            (DiagnosticSeverity::HINT, "✓ Pass")
        } else {
            (DiagnosticSeverity::ERROR, "✗ Fail")
        };

        let message = if assertion.passed {
            format!("{}: {}", prefix, assertion.assertion)
        } else {
            format!("{}: {}", prefix, assertion.reasoning)
        };

        let code = match &assertion.scope {
            AssertionScope::Storyline => Some("storyline".to_string()),
            AssertionScope::UserMessage { msg_index } => {
                Some(format!("user-msg-{}", msg_index))
            }
            AssertionScope::AssistantMessage { slot_index, .. } => {
                Some(format!("assistant-msg-{}", slot_index))
            }
            AssertionScope::OrderingMatch => Some("ordering".to_string()),
        };

        diagnostics.push(Diagnostic {
            range,
            severity: Some(severity),
            code: code.map(NumberOrString::String),
            source: Some("md-testing".to_string()),
            message,
            ..Default::default()
        });
    }

    diagnostics
}

/// Extract the test case name from a file URI.
/// e.g., file:///path/to/new_agent_loop_u2a_1t_greeting.testcase.md
///       -> "new_agent_loop_u2a_1t_greeting"
fn extract_case_name(uri: &Url) -> String {
    let path = uri.path();
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_end_matches(".testcase").to_string())
        .unwrap_or_default()
}
