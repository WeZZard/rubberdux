use crate::parser::{Message, TestCase};

/// A mapped assertion with its line number in the original file.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertionLine {
    pub msg_index: usize,
    pub line: usize,
    pub assertion: String,
}

/// Maps storyline and assistant assertions to their 1-based line numbers.
///
/// Scans the original content line-by-line to find `## Storyline` and
/// `## Assistant Message` headings, then records the line number of each
/// assertion comment within those sections.
pub fn map_assertion_lines(content: &str, test_case: &TestCase) -> Vec<AssertionLine> {
    let mut lines = Vec::new();
    let content_lines: Vec<&str> = content.lines().collect();

    // Map storyline assertions
    let mut storyline_found = false;
    let mut storyline_comment_idx = 0;
    for (i, line) in content_lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "## Storyline" {
            storyline_found = true;
            continue;
        }
        if storyline_found {
            if trimmed.starts_with("## ") && trimmed != "## Storyline" {
                break;
            }
            let line_trimmed = trimmed;
            if line_trimmed.starts_with("<!--")
                && line_trimmed.ends_with("-->")
                && storyline_comment_idx < test_case.storyline.len()
            {
                lines.push(AssertionLine {
                    msg_index: 0,
                    line: i + 1,
                    assertion: test_case.storyline[storyline_comment_idx].clone(),
                });
                storyline_comment_idx += 1;
            }
        }
    }

    // Map assistant message assertions
    let mut assistant_slot_idx = 0;
    let mut in_assistant = false;
    let mut assistant_comment_idx = 0;
    let mut current_assistant_assertions: Vec<String> = Vec::new();

    // Pre-collect assistant messages with their assertion counts
    let assistant_messages: Vec<&Vec<String>> = test_case
        .messages
        .iter()
        .filter_map(|msg| match msg {
            Message::Assistant { assertions, .. } => Some(assertions),
            _ => None,
        })
        .collect();

    for (i, line) in content_lines.iter().enumerate() {
        let trimmed = line.trim();

        // Detect assistant message headings (bare or CHECK:)
        if trimmed == "## Assistant Message" || trimmed == "## CHECK: Assistant Message" {
            in_assistant = true;
            assistant_comment_idx = 0;
            if assistant_slot_idx < assistant_messages.len() {
                current_assistant_assertions = assistant_messages[assistant_slot_idx].clone();
            }
            assistant_slot_idx += 1;
            continue;
        }

        if in_assistant {
            // End of assistant section
            if trimmed.starts_with("## ")
                && trimmed != "## Assistant Message"
                && trimmed != "## CHECK: Assistant Message"
            {
                in_assistant = false;
                continue;
            }

            let line_trimmed = trimmed;
            if line_trimmed.starts_with("<!--")
                && line_trimmed.ends_with("-->")
                && assistant_comment_idx < current_assistant_assertions.len()
            {
                lines.push(AssertionLine {
                    msg_index: assistant_slot_idx - 1,
                    line: i + 1,
                    assertion: current_assistant_assertions[assistant_comment_idx].clone(),
                });
                assistant_comment_idx += 1;
            }
        }
    }

    lines
}

/// Find the 1-based line number of a heading in the content.
pub fn find_heading_line(content: &str, heading: &str) -> Option<usize> {
    content
        .lines()
        .enumerate()
        .find(|(_, line)| {
            let trimmed = line.trim();
            trimmed == format!("## {}", heading) || trimmed == format!("## CHECK: {}", heading)
        })
        .map(|(i, _)| i + 1)
}

/// Find the 1-based line numbers of all assistant message headings.
pub fn find_assistant_heading_lines(content: &str) -> Vec<usize> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim();
            trimmed == "## Assistant Message" || trimmed == "## CHECK: Assistant Message"
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// Find the 1-based line numbers of all user message headings.
pub fn find_user_heading_lines(content: &str) -> Vec<usize> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "## User Message")
        .map(|(i, _)| i + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn test_map_assertion_lines_basic() {
        let content = r##"## Storyline
<!-- The agent should greet politely -->

## User Message
Hello!

## Assistant Message
<!-- The assistant should respond with a greeting -->
"##;

        let case = parser::parse(content, "greeting").unwrap();
        let lines = map_assertion_lines(content, &case);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, 2);
        assert_eq!(lines[0].assertion, "The agent should greet politely");
        assert_eq!(lines[1].line, 8);
        assert_eq!(
            lines[1].assertion,
            "The assistant should respond with a greeting"
        );
    }

    #[test]
    fn test_map_assertion_lines_with_check_directive() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## CHECK: Assistant Message
<!-- Should greet -->
"##;

        let case = parser::parse(content, "check").unwrap();
        let lines = map_assertion_lines(content, &case);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].line, 8);
        assert_eq!(lines[1].assertion, "Should greet");
    }

    #[test]
    fn test_find_heading_line() {
        let content = "## Storyline\n\n## User Message\n\n## Assistant Message\n";
        assert_eq!(find_heading_line(content, "Storyline"), Some(1));
        assert_eq!(find_heading_line(content, "User Message"), Some(3));
        assert_eq!(find_heading_line(content, "Assistant Message"), Some(5));
    }
}
