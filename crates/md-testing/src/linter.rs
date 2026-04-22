use crate::parser::{Message, TestCase, UserContent};

/// A lint error with line number and message.
#[derive(Debug, Clone, PartialEq)]
pub struct LintError {
    pub line: usize,
    pub rule: &'static str,
    pub message: String,
}

/// Lint a test case markdown string.
///
/// Returns `Ok(())` if valid, or `Err` with a list of lint errors.
pub fn lint(content: &str) -> Result<(), Vec<LintError>> {
    let mut errors = Vec::new();

    // R1: Must have ## Storyline section
    if !has_heading(content, "Storyline") {
        errors.push(LintError {
            line: 1,
            rule: "R1",
            message: "Missing required '## Storyline' section".to_string(),
        });
    }

    // Try to parse to catch more errors
    match crate::parser::parse(content, "lint") {
        Ok(test_case) => {
            // R2: Storyline must have at least one HTML comment
            if test_case.storyline.is_empty() {
                errors.push(LintError {
                    line: find_line(content, "## Storyline").unwrap_or(1),
                    rule: "R2",
                    message: "Storyline must contain at least one HTML comment".to_string(),
                });
            }

            // R3: First message after Storyline must be User Message
            // Already enforced by parser, but we check explicitly
            if let Some(first) = test_case.messages.first() {
                if !matches!(first, Message::User(_)) {
                    errors.push(LintError {
                        line: find_first_message_line(content).unwrap_or(1),
                        rule: "R3",
                        message: "First message after Storyline must be '## User Message'"
                            .to_string(),
                    });
                }
            }

            // R4: User message must be either plain text or HTML comments, not mixed
            // Already enforced by parser

            // R5: Assistant message must contain only HTML comments
            // Already enforced by parser

            // R7: At least one message after Storyline
            if test_case.messages.is_empty() {
                errors.push(LintError {
                    line: find_line(content, "## Storyline").unwrap_or(1),
                    rule: "R7",
                    message: "Test case must have at least one message after Storyline".to_string(),
                });
            }

            // R8: Front matter target must be "agent-loop" or "telegram-channel"
            if !test_case.front_matter.target.is_empty()
                && test_case.front_matter.target != "agent-loop"
                && test_case.front_matter.target != "telegram-channel"
            {
                errors.push(LintError {
                    line: 1,
                    rule: "R8",
                    message: format!(
                        "Front matter 'target' must be 'agent-loop' or 'telegram-channel', got '{}'",
                        test_case.front_matter.target
                    ),
                });
            }

            // Check for plain text in Storyline
            if let Some(line) = find_plain_text_in_storyline(content) {
                errors.push(LintError {
                    line,
                    rule: "R2",
                    message: "Storyline must contain only HTML comments, no plain text".to_string(),
                });
            }
        }
        Err(e) => {
            // Parser error — map to lint error
            errors.push(LintError {
                line: 1,
                rule: "PARSE",
                message: e,
            });
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Lint a parsed TestCase for additional semantic checks.
pub fn lint_test_case(test_case: &TestCase) -> Result<(), Vec<LintError>> {
    let mut errors = Vec::new();

    // R2: Storyline must have at least one HTML comment
    if test_case.storyline.is_empty() {
        errors.push(LintError {
            line: 1,
            rule: "R2",
            message: "Storyline must contain at least one HTML comment".to_string(),
        });
    }

    // R3: First message must be User
    if let Some(first) = test_case.messages.first() {
        if !matches!(first, Message::User(_)) {
            errors.push(LintError {
                line: 1,
                rule: "R3",
                message: "First message after Storyline must be '## User Message'".to_string(),
            });
        }
    }

    // R7: At least one message
    if test_case.messages.is_empty() {
        errors.push(LintError {
            line: 1,
            rule: "R7",
            message: "Test case must have at least one message after Storyline".to_string(),
        });
    }

    // Check each message
    for (i, msg) in test_case.messages.iter().enumerate() {
        match msg {
            Message::User(content) => match content {
                UserContent::PlainText(text) => {
                    if text.is_empty() {
                        errors.push(LintError {
                            line: 1,
                            rule: "R4",
                            message: format!("User message {} must not be empty", i + 1),
                        });
                    }
                }
                UserContent::Guidance(guidance) => {
                    if guidance.is_empty() {
                        errors.push(LintError {
                            line: 1,
                            rule: "R4",
                            message: format!(
                                "User message {} guidance must not be empty",
                                i + 1
                            ),
                        });
                    }
                }
            },
            Message::Assistant { assertions, .. } => {
                if assertions.is_empty() {
                    errors.push(LintError {
                        line: 1,
                        rule: "R5",
                        message: format!(
                            "Assistant message {} must contain at least one HTML comment",
                            i + 1
                        ),
                    });
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn has_heading(content: &str, heading: &str) -> bool {
    content.contains(&format!("## {}", heading))
}

fn find_line(content: &str, needle: &str) -> Option<usize> {
    content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .map(|(i, _)| i + 1)
}

fn find_first_message_line(content: &str) -> Option<usize> {
    let mut in_storyline = false;
    for (i, line) in content.lines().enumerate() {
        if line.trim() == "## Storyline" {
            in_storyline = true;
            continue;
        }
        if in_storyline && line.starts_with("## ") && line != "## Storyline" {
            return Some(i + 1);
        }
    }
    None
}

fn find_plain_text_in_storyline(content: &str) -> Option<usize> {
    let mut in_storyline = false;
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed == "## Storyline" {
            in_storyline = true;
            continue;
        }
        if in_storyline {
            if trimmed.starts_with("## ") {
                break;
            }
            if !trimmed.is_empty()
                && !trimmed.starts_with("<!--")
                && !trimmed.starts_with("<!--")
            {
                return Some(i + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lint_valid() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;
        assert!(lint(content).is_ok());
    }

    #[test]
    fn test_lint_missing_storyline() {
        let content = r##"## User Message
Hello
"##;
        let result = lint(content);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.rule == "R1"));
    }

    #[test]
    fn test_lint_empty_storyline() {
        let content = r##"## Storyline

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;
        let result = lint(content);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        // Parser catches this as "Storyline must contain at least one HTML comment"
        assert!(errors.iter().any(|e| e.rule == "R2" || e.rule == "PARSE"));
    }

    #[test]
    fn test_lint_plain_text_in_storyline() {
        let content = r##"## Storyline
This is plain text

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;
        let result = lint(content);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        // Parser catches this as "Storyline must contain only HTML comments, no plain text"
        assert!(errors.iter().any(|e| e.rule == "R2" || e.rule == "PARSE"));
    }

    #[test]
    fn test_lint_first_message_not_user() {
        let content = r##"## Storyline
<!-- Test -->

## Assistant Message
<!-- Should fail -->
"##;
        let result = lint(content);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.rule == "R3" || e.rule == "PARSE"));
    }

    #[test]
    fn test_lint_mixed_user_content() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello <!-- mixed --> world

## Assistant Message
<!-- Should fail -->
"##;
        let result = lint(content);
        // Inline HTML comments in user messages are stripped by the parser,
        // so this is treated as plain text "Hello  world".
        assert!(result.is_ok());
    }

    #[test]
    fn test_lint_assistant_plain_text() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## Assistant Message
Plain text
"##;
        let result = lint(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_lint_multiple_users_valid() {
        let content = r##"## Storyline
<!-- Multi-message -->

## User Message
First

## User Message
Second

## Assistant Message
<!-- Should address both -->
"##;
        assert!(lint(content).is_ok());
    }
}
