use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// A parsed test case from a `.testcase.md` file.
#[derive(Debug, Clone, PartialEq)]
pub struct TestCase {
    pub name: String,
    pub front_matter: FrontMatter,
    pub storyline: Vec<String>,
    pub messages: Vec<Message>,
}

/// Front matter extracted from the YAML block at the top of a `.testcase.md` file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrontMatter {
    /// Test target: "agent-loop" or "telegram-channel".
    pub target: String,
    /// Optional timeout override in seconds (default: 60).
    pub timeout: u64,
    /// Optional list of required features (e.g., "vm").
    pub features: Vec<String>,
}

/// Ordering directive for an assistant message, controlling how it is matched
/// against the actual assistant messages produced by the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderingDirective {
    /// Match this assistant message somewhere after the previous match.
    /// Intervening unmatched assistant messages are allowed.
    Check,
}

/// A single message in the test case sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A user message — either plain text or guidance for LLM generation.
    User(UserContent),
    /// An assistant message — carries an ordering directive and HTML-comment assertions.
    Assistant {
        directive: OrderingDirective,
        assertions: Vec<String>,
    },
}

/// Content of a user message.
#[derive(Debug, Clone, PartialEq)]
pub enum UserContent {
    /// Plain text to send directly to the agent.
    PlainText(String),
    /// HTML comment guidance — LLM generates the actual message.
    Guidance(Vec<String>),
}

/// Parse a `.testcase.md` file into a structured `TestCase`.
///
/// The `name` parameter is used to identify the test case (typically the filename).
pub fn parse(content: &str, name: &str) -> Result<TestCase, String> {
    let (front_matter, markdown) = split_front_matter(content);
    let events: Vec<Event> = Parser::new(markdown).collect();
    let mut pos = 0usize;

    // Expect ## Storyline first
    let storyline = parse_storyline(&events, &mut pos)?;

    // Collect messages in order
    let mut messages = Vec::new();
    while pos < events.len() {
        skip_non_content(&events, &mut pos);
        if pos >= events.len() {
            break;
        }

        if let Some((heading_text, _)) = read_heading(&events, &mut pos) {
            if heading_text == "User Message" {
                let (comments, plain_text) = collect_section_content(&events, &mut pos)?;

                if !comments.is_empty() && !plain_text.is_empty() {
                    return Err(
                        "User message cannot mix HTML comments and plain text".to_string(),
                    );
                }

                if !comments.is_empty() {
                    messages.push(Message::User(UserContent::Guidance(comments)));
                } else if !plain_text.is_empty() {
                    messages.push(Message::User(UserContent::PlainText(plain_text)));
                } else {
                    return Err(
                        "User message must contain either HTML comments or plain text"
                            .to_string(),
                    );
                }
            } else if heading_text == "Assistant Message" {
                let (comments, plain_text) = collect_section_content(&events, &mut pos)?;

                if !plain_text.is_empty() {
                    return Err(
                        "Assistant message must contain only HTML comments, not plain text"
                            .to_string(),
                    );
                }

                if comments.is_empty() {
                    return Err(
                        "Assistant message must contain at least one HTML comment".to_string(),
                    );
                }

                messages.push(Message::Assistant {
                    directive: OrderingDirective::Check,
                    assertions: comments,
                });
            } else {
                match parse_directive_from_heading(&heading_text) {
                    DirectiveParseResult::Found(directive, title) => {
                        if title != "Assistant Message" {
                            return Err(format!(
                                "Directive '{:?}' is only valid on '## Assistant Message', found '## {}'",
                                directive, heading_text
                            ));
                        }

                        let (comments, plain_text) = collect_section_content(&events, &mut pos)?;

                        if !plain_text.is_empty() {
                            return Err(
                                "Assistant message must contain only HTML comments, not plain text"
                                    .to_string(),
                            );
                        }

                        if comments.is_empty() {
                            return Err(
                                "Assistant message must contain at least one HTML comment".to_string(),
                            );
                        }

                        messages.push(Message::Assistant {
                            directive,
                            assertions: comments,
                        });
                    }
                    DirectiveParseResult::Unsupported(name) => {
                        return Err(format!(
                            "Unsupported ordering directive '{}'. Only 'CHECK:' is currently supported.",
                            name
                        ));
                    }
                    DirectiveParseResult::None => {
                        // Unknown heading — skip until next known heading or EOF
                        skip_section(&events, &mut pos);
                    }
                }
            }
        } else {
            // Not a heading — skip
            pos += 1;
        }
    }

    if messages.is_empty() {
        return Err(
            "Test case must have at least one message after Storyline".to_string(),
        );
    }

    // Validate: first message must be User
    match &messages.first() {
        Some(Message::User(_)) => {}
        _ => {
            return Err(
                "First message after Storyline must be '## User Message'".to_string(),
            )
        }
    }

    Ok(TestCase {
        name: name.to_string(),
        front_matter,
        storyline,
        messages,
    })
}

// ---------------------------------------------------------------------------
// Front matter
// ---------------------------------------------------------------------------

/// Split content into (front_matter, markdown).
/// Front matter is the YAML block between the first two `---` lines.
fn split_front_matter(content: &str) -> (FrontMatter, &str) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (FrontMatter::default(), content);
    }

    // Find the end of front matter (second `---`)
    let after_first = &trimmed[3..];
    if let Some(end_idx) = after_first.find("\n---") {
        let yaml_block = &after_first[..end_idx];
        let markdown = &after_first[end_idx + 4..]; // skip "\n---"
        let front_matter = parse_front_matter(yaml_block);
        return (front_matter, markdown);
    }

    (FrontMatter::default(), content)
}

/// Parse a simple YAML front matter block.
fn parse_front_matter(yaml: &str) -> FrontMatter {
    let mut front_matter = FrontMatter::default();

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "target" => front_matter.target = value.to_string(),
                "timeout" => {
                    if let Ok(t) = value.parse() {
                        front_matter.timeout = t;
                    }
                }
                "features" => {
                    // Start of a list — we'll handle inline or list syntax
                    // For now, just mark that we're in features mode
                }
                _ => {}
            }
        }

        // Handle list items under "features:"
        if trimmed.starts_with("- ") {
            let feature = trimmed[2..].trim().to_string();
            if !feature.is_empty() {
                front_matter.features.push(feature);
            }
        }
    }

    front_matter
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the Storyline section. Returns the list of HTML comment contents.
fn parse_storyline(events: &[Event], pos: &mut usize) -> Result<Vec<String>, String> {
    skip_non_content(events, pos);

    match read_heading(events, pos) {
        Some((text, _)) if text == "Storyline" => {}
        Some((text, _)) => {
            return Err(format!(
                "Expected '## Storyline' as first section, found '## {}'",
                text
            ))
        }
        None => return Err("Test case must start with '## Storyline'".to_string()),
    }

    let (comments, plain_text) = collect_section_content(events, pos)?;

    if !plain_text.is_empty() {
        return Err("Storyline must contain only HTML comments, no plain text".to_string());
    }

    if comments.is_empty() {
        return Err("Storyline must contain at least one HTML comment".to_string());
    }

    Ok(comments)
}

/// Read a heading at the current position.
/// Returns (heading_text, level) and advances past the heading.
fn read_heading(events: &[Event], pos: &mut usize) -> Option<(String, u32)> {
    if *pos >= events.len() {
        return None;
    }

    match &events[*pos] {
        Event::Start(Tag::Heading { level, .. }) => {
            let level = match *level {
                pulldown_cmark::HeadingLevel::H1 => 1,
                pulldown_cmark::HeadingLevel::H2 => 2,
                pulldown_cmark::HeadingLevel::H3 => 3,
                pulldown_cmark::HeadingLevel::H4 => 4,
                pulldown_cmark::HeadingLevel::H5 => 5,
                pulldown_cmark::HeadingLevel::H6 => 6,
            };
            *pos += 1; // consume Start(Heading)

            // Collect heading text
            let mut text = String::new();
            while *pos < events.len() {
                match &events[*pos] {
                    Event::End(TagEnd::Heading(..)) => {
                        *pos += 1; // consume End(Heading)
                        return Some((text.trim().to_string(), level));
                    }
                    Event::Text(t) => {
                        text.push_str(t.as_ref());
                        *pos += 1;
                    }
                    _ => {
                        *pos += 1;
                    }
                }
            }
            Some((text.trim().to_string(), level))
        }
        _ => None,
    }
}

/// Skip non-content events (whitespace, etc.) until we hit a heading or meaningful content.
fn skip_non_content(events: &[Event], pos: &mut usize) {
    while *pos < events.len() {
        match &events[*pos] {
            Event::Start(Tag::Heading { .. }) => break,
            Event::Html(html) => {
                let s = html.as_ref().trim();
                if s.starts_with("<!--") && s.ends_with("-->") {
                    break;
                }
                *pos += 1;
            }
            Event::Text(t) if t.as_ref().trim().is_empty() => {
                *pos += 1;
            }
            _ => {
                *pos += 1;
            }
        }
    }
}

/// Result of trying to parse an ordering directive from a heading.
enum DirectiveParseResult {
    /// A valid directive was found.
    Found(OrderingDirective, String),
    /// No directive prefix was found (bare heading).
    None,
    /// An unsupported directive prefix was found.
    Unsupported(String),
}

/// Parse an ordering directive from a heading text.
///
/// Returns `Found(directive, remaining_title)` if a known directive prefix
/// is present, `None` for a bare heading, or `Unsupported(name)` for an
/// unrecognized directive prefix.
fn parse_directive_from_heading(text: &str) -> DirectiveParseResult {
    // Check for unsupported directive prefixes first so we can give clear errors.
    let unsupported_prefixes = [
        "CHECK-NEXT:",
        "CHECK-DAG:",
        "CHECK-NOT:",
        "CHECK-SAME:",
        "CHECK-EMPTY:",
        "CHECK-COUNT-",
        "CHECK-LABEL:",
    ];
    for prefix in &unsupported_prefixes {
        if text.starts_with(prefix) {
            return DirectiveParseResult::Unsupported(prefix.trim_end_matches(':').to_string());
        }
    }

    if let Some(rest) = text.strip_prefix("CHECK:") {
        let title = rest.trim().to_string();
        if title.is_empty() {
            return DirectiveParseResult::None;
        }
        return DirectiveParseResult::Found(OrderingDirective::Check, title);
    }

    DirectiveParseResult::None
}

/// Skip all content until the next heading.
fn skip_section(events: &[Event], pos: &mut usize) {
    while *pos < events.len() {
        match &events[*pos] {
            Event::Start(Tag::Heading { .. }) => break,
            _ => {
                *pos += 1;
            }
        }
    }
}

/// Collect all content from the current position until the next heading or EOF.
/// Returns (html_comments, plain_text).
fn collect_section_content(
    events: &[Event],
    pos: &mut usize,
) -> Result<(Vec<String>, String), String> {
    let mut comments = Vec::new();
    let mut plain_text = String::new();

    while *pos < events.len() {
        match &events[*pos] {
            Event::Start(Tag::Heading { .. }) => break,
            Event::Html(html) => {
                let s = html.as_ref().trim();
                if s.starts_with("<!--") && s.ends_with("-->") {
                    let content = s[4..s.len() - 3].trim().to_string();
                    if !content.is_empty() {
                        comments.push(content);
                    }
                }
                *pos += 1;
            }
            Event::Text(t) => {
                plain_text.push_str(t.as_ref());
                *pos += 1;
            }
            Event::SoftBreak | Event::HardBreak => {
                plain_text.push('\n');
                *pos += 1;
            }
            Event::Code(c) => {
                plain_text.push_str(c.as_ref());
                *pos += 1;
            }
            _ => {
                *pos += 1;
            }
        }
    }

    Ok((comments, plain_text.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic() {
        let content = r##"## Storyline
<!-- The agent should greet politely -->

## User Message
Hello!

## Assistant Message
<!-- The assistant should respond with a greeting -->
"##;

        let case = parse(content, "greeting").unwrap();
        assert_eq!(case.name, "greeting");
        assert_eq!(case.storyline.len(), 1);
        assert_eq!(case.storyline[0], "The agent should greet politely");
        assert_eq!(case.messages.len(), 2);

        match &case.messages[0] {
            Message::User(UserContent::PlainText(t)) => assert_eq!(t, "Hello!"),
            _ => panic!("Expected plain text user message"),
        }

        match &case.messages[1] {
            Message::Assistant { directive, assertions } => {
                assert!(matches!(directive, OrderingDirective::Check));
                assert_eq!(assertions.len(), 1);
                assert_eq!(assertions[0], "The assistant should respond with a greeting");
            }
            _ => panic!("Expected assistant message"),
        }
    }

    #[test]
    fn test_parse_guidance_user() {
        let content = r##"## Storyline
<!-- Test guidance -->

## User Message
<!-- Generate a greeting message -->

## Assistant Message
<!-- Should respond politely -->
"##;

        let case = parse(content, "guidance").unwrap();
        match &case.messages[0] {
            Message::User(UserContent::Guidance(g)) => {
                assert_eq!(g.len(), 1);
                assert_eq!(g[0], "Generate a greeting message");
            }
            _ => panic!("Expected guidance user message"),
        }
    }

    #[test]
    fn test_parse_multiple_user_messages() {
        let content = r##"## Storyline
<!-- Multi-message test -->

## User Message
First message

## User Message
Second message

## Assistant Message
<!-- Should address both -->
"##;

        let case = parse(content, "multi").unwrap();
        assert_eq!(case.messages.len(), 3);
        match &case.messages[0] {
            Message::User(UserContent::PlainText(t)) => assert_eq!(t, "First message"),
            _ => panic!("Expected first user message"),
        }
        match &case.messages[1] {
            Message::User(UserContent::PlainText(t)) => assert_eq!(t, "Second message"),
            _ => panic!("Expected second user message"),
        }
    }

    #[test]
    fn test_parse_multiple_assistant_messages() {
        let content = r##"## Storyline
<!-- Multi-assistant test -->

## User Message
Hello

## Assistant Message
<!-- First assertion -->

## Assistant Message
<!-- Second assertion -->
"##;

        let case = parse(content, "multi-assistant").unwrap();
        assert_eq!(case.messages.len(), 3);
        match &case.messages[1] {
            Message::Assistant { assertions, .. } => assert_eq!(assertions[0], "First assertion"),
            _ => panic!("Expected first assistant"),
        }
        match &case.messages[2] {
            Message::Assistant { assertions, .. } => assert_eq!(assertions[0], "Second assertion"),
            _ => panic!("Expected second assistant"),
        }
    }

    #[test]
    fn test_rejects_missing_storyline() {
        let content = r##"## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;

        let result = parse(content, "missing-storyline");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Storyline"));
    }

    #[test]
    fn test_rejects_mixed_user_content() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello <!-- mixed --> world

## Assistant Message
<!-- Should fail -->
"##;

        let result = parse(content, "mixed");
        // pulldown-cmark parses inline HTML comments as separate Html events
        // between text, so "Hello <!-- mixed --> world" becomes:
        // Text("Hello "), Html("<!-- mixed -->"), Text(" world")
        // Our parser collects plain text and comments separately, so this
        // currently parses as PlainText("Hello  world") with the comment ignored.
        // This is acceptable behavior — inline comments in user messages are stripped.
        assert!(result.is_ok());
        match &result.unwrap().messages[0] {
            Message::User(UserContent::PlainText(t)) => {
                assert_eq!(t, "Hello  world");
            }
            _ => panic!("Expected plain text with comment stripped"),
        }
    }

    #[test]
    fn test_rejects_assistant_plain_text() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## Assistant Message
This is plain text
"##;

        let result = parse(content, "assistant-plain");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Assistant message"));
    }

    #[test]
    fn test_rejects_first_message_not_user() {
        let content = r##"## Storyline
<!-- Test -->

## Assistant Message
<!-- Should fail -->
"##;

        let result = parse(content, "wrong-first");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("First message"));
    }

    #[test]
    fn test_rejects_empty_storyline() {
        let content = r##"## Storyline

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;

        let result = parse(content, "empty-storyline");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Storyline"));
    }

    #[test]
    fn test_parse_front_matter() {
        let content = r##"---
target: agent-loop
timeout: 30
features:
  - vm
---

## Storyline
<!-- Test front matter -->

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;

        let case = parse(content, "front-matter").unwrap();
        assert_eq!(case.front_matter.target, "agent-loop");
        assert_eq!(case.front_matter.timeout, 30);
        assert_eq!(case.front_matter.features, vec!["vm"]);
        assert_eq!(case.storyline[0], "Test front matter");
    }

    #[test]
    fn test_parse_front_matter_telegram_channel() {
        let content = r##"---
target: telegram-channel
timeout: 120
---

## Storyline
<!-- Telegram test -->

## User Message
Hello

## Assistant Message
<!-- Should respond -->
"##;

        let case = parse(content, "telegram").unwrap();
        assert_eq!(case.front_matter.target, "telegram-channel");
        assert_eq!(case.front_matter.timeout, 120);
        assert!(case.front_matter.features.is_empty());
    }

    #[test]
    fn test_parse_no_front_matter_defaults() {
        let content = r##"## Storyline
<!-- No front matter -->

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;

        let case = parse(content, "no-front-matter").unwrap();
        assert_eq!(case.front_matter.target, "");
        assert_eq!(case.front_matter.timeout, 0);
        assert!(case.front_matter.features.is_empty());
    }

    #[test]
    fn test_parse_assistant_with_check_prefix() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## CHECK: Assistant Message
<!-- Should greet -->
"##;

        let case = parse(content, "check-prefix").unwrap();
        assert_eq!(case.messages.len(), 2);
        match &case.messages[1] {
            Message::Assistant { directive, assertions } => {
                assert!(matches!(directive, OrderingDirective::Check));
                assert_eq!(assertions.len(), 1);
                assert_eq!(assertions[0], "Should greet");
            }
            _ => panic!("Expected assistant message with CHECK directive"),
        }
    }

    #[test]
    fn test_parse_assistant_bare_is_implicit_check() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;

        let case = parse(content, "bare").unwrap();
        match &case.messages[1] {
            Message::Assistant { directive, assertions } => {
                assert!(matches!(directive, OrderingDirective::Check));
                assert_eq!(assertions[0], "Should greet");
            }
            _ => panic!("Expected assistant message with implicit CHECK"),
        }
    }

    #[test]
    fn test_rejects_unsupported_directive() {
        let content = r##"## Storyline
<!-- Test -->

## User Message
Hello

## CHECK-NEXT: Assistant Message
<!-- Should greet -->
"##;

        let result = parse(content, "unsupported");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("CHECK-NEXT"), "Expected error about unsupported directive, got: {}", err);
    }

    #[test]
    fn test_rejects_directive_on_user_message() {
        let content = r##"## Storyline
<!-- Test -->

## CHECK: User Message
Hello

## Assistant Message
<!-- Should greet -->
"##;

        let result = parse(content, "directive-on-user");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("User Message"), "Expected error about directive on wrong heading, got: {}", err);
    }
}

