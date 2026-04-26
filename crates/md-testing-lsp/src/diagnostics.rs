use std::path::Path;

use md_testing::{AssertionScope, TestResults, lint};
use tower_lsp::lsp_types::*;

use crate::code_actions::build_icon_diagnostic;
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

fn build_result_diagnostics(content: &str, results: &TestResults) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Re-parse current content to get updated line numbers
    let current_lines = if let Ok(test_case) = md_testing::parser::parse(content, "") {
        md_testing::map_assertion_lines(content, &test_case)
    } else {
        Vec::new()
    };

    for assertion in &results.assertions {
        // Try to find the current line number by matching assertion text
        let line = match &assertion.scope {
            AssertionScope::FrontMatter { key } => {
                md_testing::find_front_matter_key_line(content, key).unwrap_or(assertion.line)
            }
            _ => current_lines
                .iter()
                .find(|l| l.assertion == assertion.assertion)
                .map(|l| l.line)
                .unwrap_or(assertion.line),
        };

        let mut assertion = assertion.clone();
        assertion.line = line;
        diagnostics.push(build_icon_diagnostic(&assertion, results));
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
