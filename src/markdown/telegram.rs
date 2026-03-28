use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use telegram_markdown_v2::UnsupportedTagsStrategy;

/// Formats standard markdown for Telegram MarkdownV2.
///
/// Stage 1: Pre-processes unsupported elements (tables → block-per-row lists).
/// Stage 2: Converts to Telegram MarkdownV2 via `telegram-markdown-v2`.
pub fn format(input: &str) -> String {
    let preprocessed = preprocess_tables(input);
    telegram_markdown_v2::convert_with_strategy(&preprocessed, UnsupportedTagsStrategy::Escape)
        .unwrap_or_else(|_| escape_fallback(&preprocessed))
}

fn escape_fallback(input: &str) -> String {
    let special = ['_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!'];
    let mut out = String::with_capacity(input.len() * 2);
    for ch in input.chars() {
        if special.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn preprocess_tables(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, options);
    let events: Vec<(Event, std::ops::Range<usize>)> = parser.into_offset_iter().collect();

    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    let mut last_end: usize = 0;

    while i < events.len() {
        match &events[i] {
            (Event::Start(Tag::Table(_)), range) => {
                // Append any content before this table
                result.push_str(&input[last_end..range.start]);

                let (table_text, new_i, table_range_end) =
                    collect_table(&events, i);
                result.push_str(&table_text);

                last_end = table_range_end;
                i = new_i;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Append any remaining content after the last table
    result.push_str(&input[last_end..]);
    result
}

fn collect_table(
    events: &[(Event, std::ops::Range<usize>)],
    start: usize,
) -> (String, usize, usize) {
    let mut headers: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current_cell = String::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut in_head = false;
    let mut i = start + 1;
    let mut table_end_offset = events[start].1.end;

    while i < events.len() {
        match &events[i] {
            (Event::Start(Tag::TableHead), _) => {
                in_head = true;
                current_row.clear();
            }
            (Event::End(TagEnd::TableHead), _) => {
                headers = current_row.clone();
                current_row.clear();
                in_head = false;
            }
            (Event::Start(Tag::TableRow), _) => {
                current_row.clear();
            }
            (Event::End(TagEnd::TableRow), _) => {
                if !in_head {
                    rows.push(current_row.clone());
                }
                current_row.clear();
            }
            (Event::Start(Tag::TableCell), _) => {
                current_cell.clear();
            }
            (Event::End(TagEnd::TableCell), _) => {
                current_row.push(current_cell.trim().to_owned());
                current_cell.clear();
            }
            (Event::Text(text), _) => {
                current_cell.push_str(text);
            }
            (Event::Code(code), _) => {
                current_cell.push('`');
                current_cell.push_str(code);
                current_cell.push('`');
            }
            (Event::SoftBreak | Event::HardBreak, _) => {
                current_cell.push(' ');
            }
            (Event::End(TagEnd::Table), range) => {
                table_end_offset = range.end;
                i += 1;
                break;
            }
            _ => {}
        }
        i += 1;
    }

    let text = render_table_as_blocks(&headers, &rows);
    (text, i, table_end_offset)
}

fn render_table_as_blocks(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();

    if headers.len() <= 1 {
        // Single-column: render as bullet points
        for row in rows {
            if let Some(val) = row.first() {
                out.push_str("- ");
                out.push_str(val);
                out.push('\n');
            }
        }
        return out;
    }

    for row in rows {
        let title = row.first().map(|s| s.as_str()).unwrap_or("(unknown)");
        out.push_str(&format!("**{}**\n", title));

        for (header, value) in headers.iter().zip(row.iter()) {
            out.push_str(&format!("  {}: {}\n", header, value));
        }
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_passthrough() {
        let input = "Hello, world!";
        let result = preprocess_tables(input);
        assert!(result.contains("Hello, world!"));
    }

    #[test]
    fn test_table_to_block_list() {
        let input = "| Name | Role |\n|------|------|\n| Alice | Engineer |\n| Bob | Designer |\n";
        let result = preprocess_tables(input);

        assert!(result.contains("**Alice**"));
        assert!(result.contains("  Name: Alice"));
        assert!(result.contains("  Role: Engineer"));
        assert!(result.contains("**Bob**"));
        assert!(result.contains("  Name: Bob"));
        assert!(result.contains("  Role: Designer"));
        assert!(!result.contains('|'));
    }

    #[test]
    fn test_non_table_markdown_preserved() {
        let input = "**bold** and _italic_ and `code`";
        let result = preprocess_tables(input);
        assert!(result.contains("**bold**"));
        assert!(result.contains("_italic_"));
        assert!(result.contains("`code`"));
    }

    #[test]
    fn test_mixed_content() {
        let input = "Before table\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\nAfter table\n";
        let result = preprocess_tables(input);

        assert!(result.contains("Before table"));
        assert!(result.contains("**1**"));
        assert!(result.contains("  A: 1"));
        assert!(result.contains("  B: 2"));
        assert!(result.contains("After table"));
    }

    #[test]
    fn test_code_block_indentation_preserved() {
        let input = "```json\n{\n  \"name\": \"test\",\n  \"value\": 42\n}\n```\n";
        let result = format(input);
        println!("CODE BLOCK RESULT (repr):\n{:?}", result);
        // Indentation must be preserved inside code blocks
        assert!(result.contains("  \"name\""), "Indentation should be preserved in code blocks");
    }

    #[test]
    fn test_python_code_block_indentation() {
        let input = "```python\ndef hello():\n    print(\"world\")\n    if True:\n        return 1\n```\n";
        let result = format(input);
        println!("PYTHON BLOCK RESULT (repr):\n{:?}", result);
        assert!(result.contains("    print"), "4-space indentation should be preserved");
        assert!(result.contains("        return"), "8-space indentation should be preserved");
    }

    #[test]
    fn test_single_column_table() {
        let input = "| Items |\n|-------|\n| Apple |\n| Banana |\n";
        let result = preprocess_tables(input);

        assert!(result.contains("- Apple"));
        assert!(result.contains("- Banana"));
        assert!(!result.contains(':'));
    }
}
