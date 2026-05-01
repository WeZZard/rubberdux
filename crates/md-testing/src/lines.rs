use crate::parser::{Assertion, Message, TestCase};

/// A mapped assertion with its line number in the original file.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertionLine {
    pub msg_index: usize,
    /// 1-based line number in the source file.
    pub line: usize,
    pub assertion: String,
}

/// Maps storyline and slot assertions to their 1-based line numbers.
///
/// Scans the original content line-by-line to find `## Storyline`,
/// `## Assistant Message`, and `## Tool Call` headings, then records the
/// line number of each assertion comment within those sections.
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

    // Map slot assertions (Assistant Message + Tool Call)
    let mut slot_idx = 0;
    let mut in_slot = false;
    let mut assertion_idx = 0;
    let mut current_assertions: Vec<String> = Vec::new();

    let slot_messages: Vec<&Vec<Assertion>> = test_case
        .messages
        .iter()
        .filter_map(|msg| match msg {
            Message::Assistant { assertions, .. } | Message::ToolCall { assertions, .. } => {
                Some(assertions)
            }
            _ => None,
        })
        .collect();

    let mut in_cel_block = false;
    let mut cel_block_start: usize = 0; // 0-based index from enumerate()
    let mut cel_expression = String::new();

    for (i, line) in content_lines.iter().enumerate() {
        let trimmed = line.trim();

        if is_slot_heading(trimmed) {
            in_slot = true;
            in_cel_block = false;
            assertion_idx = 0;
            if slot_idx < slot_messages.len() {
                current_assertions = slot_messages[slot_idx]
                    .iter()
                    .map(|a| a.display_text().to_string())
                    .collect();
            }
            slot_idx += 1;
            continue;
        }

        if in_slot {
            if trimmed.starts_with("## ") && !is_slot_heading(trimmed) {
                in_slot = false;
                in_cel_block = false;
                continue;
            }

            if trimmed == "```cel" {
                in_cel_block = true;
                cel_block_start = i;
                cel_expression.clear();
                continue;
            }

            if in_cel_block {
                if trimmed == "```" {
                    in_cel_block = false;
                    let expr = cel_expression.trim().to_string();
                    if assertion_idx < current_assertions.len()
                        && current_assertions[assertion_idx] == expr
                    {
                        lines.push(AssertionLine {
                            msg_index: slot_idx - 1,
                            line: cel_block_start + 2,
                            assertion: expr,
                        });
                        assertion_idx += 1;
                    }
                } else {
                    if !cel_expression.is_empty() {
                        cel_expression.push('\n');
                    }
                    cel_expression.push_str(line);
                }
                continue;
            }

            if trimmed.starts_with("<!--")
                && trimmed.ends_with("-->")
                && assertion_idx < current_assertions.len()
            {
                let comment = trimmed[4..trimmed.len() - 3].trim();
                if current_assertions[assertion_idx] == comment {
                    lines.push(AssertionLine {
                        msg_index: slot_idx - 1,
                        line: i + 1,
                        assertion: comment.to_string(),
                    });
                    assertion_idx += 1;
                }
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

/// Find the 1-based line number of a front-matter key.
pub fn find_front_matter_key_line(content: &str, key: &str) -> Option<usize> {
    let mut lines = content.lines().enumerate();
    if lines.next()?.1.trim() != "---" {
        return None;
    }

    for (index, line) in lines {
        let trimmed = line.trim_start();
        if trimmed == "---" {
            return None;
        }
        if trimmed
            .split_once(':')
            .is_some_and(|(candidate, _)| candidate.trim() == key)
        {
            return Some(index + 1);
        }
    }

    None
}

/// Find the 1-based line numbers of all slot headings
/// (`## Assistant Message`, `## Tool Call`, and their `CHECK:` variants).
pub fn find_slot_heading_lines(content: &str) -> Vec<usize> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| is_slot_heading(line.trim()))
        .map(|(i, _)| i + 1)
        .collect()
}

/// Find the 1-based line numbers of all assistant message headings.
#[deprecated(note = "use find_slot_heading_lines instead")]
pub fn find_assistant_heading_lines(content: &str) -> Vec<usize> {
    find_slot_heading_lines(content)
}

fn is_slot_heading(trimmed: &str) -> bool {
    trimmed == "## Assistant Message"
        || trimmed == "## CHECK: Assistant Message"
        || trimmed == "## Tool Call"
        || trimmed == "## CHECK: Tool Call"
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

    #[test]
    fn test_find_front_matter_key_line() {
        let content = "---\ntimeout : 120\ntarget: agent-loop\n---\n\n## Storyline\n";

        assert_eq!(find_front_matter_key_line(content, "timeout"), Some(2));
        assert_eq!(find_front_matter_key_line(content, "target"), Some(3));
        assert_eq!(find_front_matter_key_line(content, "missing"), None);
    }

    #[test]
    fn test_find_front_matter_key_line_without_front_matter() {
        assert_eq!(
            find_front_matter_key_line("## Storyline\n", "timeout"),
            None
        );
    }

    #[test]
    fn test_map_assertion_lines_with_tool_call() {
        let content = "## Storyline\n<!-- Test -->\n\n## User Message\nCreate a file\n\n## Tool Call\n<!-- Should use write_file -->\n\n## Assistant Message\n<!-- Should confirm -->\n";

        let case = parser::parse(content, "tool-call").unwrap();
        let lines = map_assertion_lines(content, &case);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line, 2);
        assert_eq!(lines[0].assertion, "Test");
        assert_eq!(lines[1].line, 8);
        assert_eq!(lines[1].assertion, "Should use write_file");
        assert_eq!(lines[2].line, 11);
        assert_eq!(lines[2].assertion, "Should confirm");
    }

    #[test]
    fn test_map_assertion_lines_cel_points_to_expression() {
        let content = "## Storyline\n<!-- Test -->\n\n## User Message\nHello\n\n## Tool Call\n\n```cel\nmessage.tool_calls.size() > 0\n```\n\n## Assistant Message\n<!-- Should confirm -->\n";

        let case = parser::parse(content, "cel-line").unwrap();
        let lines = map_assertion_lines(content, &case);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].line, 10);
        assert_eq!(lines[1].assertion, "message.tool_calls.size() > 0");
    }

    #[test]
    fn test_find_slot_heading_lines() {
        let content = "## Storyline\n\n## User Message\n\n## Tool Call\n\n## Assistant Message\n";
        let lines = find_slot_heading_lines(content);
        assert_eq!(lines, vec![5, 7]);
    }

    #[test]
    fn test_find_slot_heading_lines_excludes_system_message() {
        let content =
            "## Storyline\n\n## System Message\n\n## User Message\n\n## Tool Call\n\n## Assistant Message\n";
        let lines = find_slot_heading_lines(content);
        assert_eq!(lines, vec![7, 9]);
    }

    #[test]
    #[allow(deprecated)]
    fn test_find_assistant_heading_lines_backward_compat() {
        let content = "## Storyline\n\n## User Message\n\n## Tool Call\n\n## Assistant Message\n";
        let lines = find_assistant_heading_lines(content);
        assert_eq!(lines, vec![5, 7]);
    }
}
