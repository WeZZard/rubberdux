/// A parsed document: sequence of top-level nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub nodes: Vec<Node>,
}

/// A top-level node in model output.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Message(MessageElement),
    Reaction(ReactionElement),
    Text(String),
}

/// `<telegram-message from="..." to="..." id="..." date="...">content</telegram-message>`
#[derive(Debug, Clone, PartialEq)]
pub struct MessageElement {
    pub from: String,
    pub to: String,
    pub id: Option<String>,
    pub date: Option<String>,
    pub content: String,
}

/// `<telegram-reaction from="..." action="..." emoji="..." message-id="..." date="..." />`
#[derive(Debug, Clone, PartialEq)]
pub struct ReactionElement {
    pub from: String,
    pub action: String,
    pub emoji: String,
    pub message_id: String,
    pub date: Option<String>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parses a string containing the Telegram markup dialect into a `Document`.
///
/// Code blocks (fenced and inline) are treated as opaque text. Tags that fail
/// to parse are emitted as literal text. All `<telegram-message>` and
/// `<telegram-reaction>` tags are parsed regardless of `from` value — filtering
/// by direction is the caller's responsibility.
pub fn parse(input: &str) -> Document {
    let mut nodes: Vec<Node> = Vec::new();
    let mut text_buf = String::new();
    let mut pos = 0;

    while pos < input.len() {
        let remaining = &input[pos..];

        // Fenced code block (```)
        if remaining.starts_with("```") {
            let start = pos;
            pos += 3;
            // Skip to end of language identifier line
            while pos < input.len() && input.as_bytes()[pos] != b'\n' {
                pos += 1;
            }
            // Find closing ```
            loop {
                if pos >= input.len() {
                    break;
                }
                if input[pos..].starts_with("```") {
                    pos += 3;
                    break;
                }
                pos += 1;
            }
            text_buf.push_str(&input[start..pos]);
            continue;
        }

        // Inline code (`)
        if remaining.starts_with('`') {
            let start = pos;
            pos += 1;
            while pos < input.len() && input.as_bytes()[pos] != b'`' {
                pos += 1;
            }
            if pos < input.len() {
                pos += 1; // closing `
            }
            text_buf.push_str(&input[start..pos]);
            continue;
        }

        // <telegram-message ...>...</telegram-message>
        if remaining.starts_with("<telegram-message ") || remaining.starts_with("<telegram-message>") {
            if let Some((el, consumed)) = parse_message_tag(remaining) {
                flush_text(&mut text_buf, &mut nodes);
                nodes.push(Node::Message(el));
                pos += consumed;
                continue;
            }
            // Failed parse — emit '<' as literal
            text_buf.push('<');
            pos += 1;
            continue;
        }

        // <telegram-reaction ... />
        if remaining.starts_with("<telegram-reaction ") {
            if let Some((el, consumed)) = parse_reaction_tag(remaining) {
                flush_text(&mut text_buf, &mut nodes);
                nodes.push(Node::Reaction(el));
                pos += consumed;
                continue;
            }
            text_buf.push('<');
            pos += 1;
            continue;
        }

        // Regular character
        let ch = remaining.chars().next().unwrap();
        text_buf.push(ch);
        pos += ch.len_utf8();
    }

    flush_text(&mut text_buf, &mut nodes);
    Document { nodes }
}

fn flush_text(buf: &mut String, nodes: &mut Vec<Node>) {
    if !buf.is_empty() {
        nodes.push(Node::Text(std::mem::take(buf)));
    }
}

/// Parses `<telegram-message ...>content</telegram-message>` at the start of `input`.
fn parse_message_tag(input: &str) -> Option<(MessageElement, usize)> {
    // Find end of opening tag
    let tag_close = input.find('>')?;
    let opening_tag = &input[..tag_close];

    // Must not be self-closing
    if opening_tag.ends_with('/') {
        return None;
    }

    let attrs = parse_attributes(opening_tag);

    let content_start = tag_close + 1;
    let end_tag = "</telegram-message>";
    let end_pos = input[content_start..].find(end_tag)?;
    let content = &input[content_start..content_start + end_pos];
    let total = content_start + end_pos + end_tag.len();

    Some((
        MessageElement {
            from: attr_value(&attrs, "from")?,
            to: attr_value(&attrs, "to")?,
            id: attr_value(&attrs, "id"),
            date: attr_value(&attrs, "date"),
            content: content.to_owned(),
        },
        total,
    ))
}

/// Parses `<telegram-reaction ... />` at the start of `input`.
fn parse_reaction_tag(input: &str) -> Option<(ReactionElement, usize)> {
    let end = input.find("/>")?;
    let tag_content = &input[..end];
    let attrs = parse_attributes(tag_content);
    let total = end + 2;

    Some((
        ReactionElement {
            from: attr_value(&attrs, "from")?,
            action: attr_value(&attrs, "action")?,
            emoji: attr_value(&attrs, "emoji")?,
            message_id: attr_value(&attrs, "message-id")?,
            date: attr_value(&attrs, "date"),
        },
        total,
    ))
}

/// Extracts all `key="value"` pairs from a tag string.
fn parse_attributes(tag: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut pos = 0;
    let bytes = tag.as_bytes();

    while pos < bytes.len() {
        // Skip whitespace
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        // Read attribute name (alphanumeric + hyphen)
        let name_start = pos;
        while pos < bytes.len()
            && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'-')
        {
            pos += 1;
        }
        if pos == name_start {
            // Not at an attribute name — skip character
            pos += 1;
            continue;
        }
        let name = &tag[name_start..pos];

        // Expect ="
        if pos + 1 >= bytes.len() || bytes[pos] != b'=' || bytes[pos + 1] != b'"' {
            continue;
        }
        pos += 2; // skip ="

        // Read attribute value until closing "
        let value_start = pos;
        while pos < bytes.len() && bytes[pos] != b'"' {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let value = &tag[value_start..pos];
        pos += 1; // skip closing "

        attrs.push((name.to_owned(), value.to_owned()));
    }

    attrs
}

fn attr_value(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

/// Serializes a `Document` back into the Telegram markup dialect.
pub fn serialize(doc: &Document) -> String {
    let mut out = String::new();
    for node in &doc.nodes {
        serialize_node_into(node, &mut out);
    }
    out
}

/// Serializes a single `Node` into a string.
pub fn serialize_node(node: &Node) -> String {
    let mut out = String::new();
    serialize_node_into(node, &mut out);
    out
}

fn serialize_node_into(node: &Node, out: &mut String) {
    match node {
        Node::Text(t) => out.push_str(t),
        Node::Message(el) => serialize_message(el, out),
        Node::Reaction(el) => serialize_reaction(el, out),
    }
}

fn serialize_message(el: &MessageElement, out: &mut String) {
    out.push_str("<telegram-message");
    push_attr(out, "from", &el.from);
    push_attr(out, "to", &el.to);
    if let Some(id) = &el.id {
        push_attr(out, "id", id);
    }
    if let Some(date) = &el.date {
        push_attr(out, "date", date);
    }
    out.push('>');
    out.push_str(&el.content);
    out.push_str("</telegram-message>");
}

fn serialize_reaction(el: &ReactionElement, out: &mut String) {
    out.push_str("<telegram-reaction");
    push_attr(out, "from", &el.from);
    push_attr(out, "action", &el.action);
    push_attr(out, "emoji", &el.emoji);
    push_attr(out, "message-id", &el.message_id);
    if let Some(date) = &el.date {
        push_attr(out, "date", date);
    }
    out.push_str(" />");
}

fn push_attr(out: &mut String, key: &str, value: &str) {
    out.push(' ');
    out.push_str(key);
    out.push_str("=\"");
    out.push_str(value);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_text() {
        let doc = parse("Just thinking here.");
        assert_eq!(doc.nodes.len(), 1);
        assert!(matches!(&doc.nodes[0], Node::Text(t) if t == "Just thinking here."));
    }

    #[test]
    fn test_parse_message_with_all_attrs() {
        let input = "<telegram-message from=\"user\" to=\"assistant\" id=\"5\" date=\"1234567890\">Hello</telegram-message>";
        let doc = parse(input);
        assert_eq!(doc.nodes.len(), 1);
        let Node::Message(el) = &doc.nodes[0] else { panic!("expected Message") };
        assert_eq!(el.from, "user");
        assert_eq!(el.to, "assistant");
        assert_eq!(el.id.as_deref(), Some("5"));
        assert_eq!(el.date.as_deref(), Some("1234567890"));
        assert_eq!(el.content, "Hello");
    }

    #[test]
    fn test_parse_message_without_optional_attrs() {
        let input = "<telegram-message from=\"assistant\" to=\"user\">Hello!</telegram-message>";
        let doc = parse(input);
        assert_eq!(doc.nodes.len(), 1);
        let Node::Message(el) = &doc.nodes[0] else { panic!("expected Message") };
        assert_eq!(el.id, None);
        assert_eq!(el.date, None);
        assert_eq!(el.content, "Hello!");
    }

    #[test]
    fn test_parse_reaction() {
        let input = "<telegram-reaction from=\"assistant\" action=\"add\" emoji=\"👍\" message-id=\"42\" />";
        let doc = parse(input);
        assert_eq!(doc.nodes.len(), 1);
        let Node::Reaction(el) = &doc.nodes[0] else { panic!("expected Reaction") };
        assert_eq!(el.from, "assistant");
        assert_eq!(el.action, "add");
        assert_eq!(el.emoji, "👍");
        assert_eq!(el.message_id, "42");
        assert_eq!(el.date, None);
    }

    #[test]
    fn test_parse_code_block_opaque() {
        let input = "```xml\n<telegram-message from=\"assistant\" to=\"user\">Not real</telegram-message>\n```";
        let doc = parse(input);
        assert!(!doc.nodes.iter().any(|n| matches!(n, Node::Message(_))));
    }

    #[test]
    fn test_parse_inline_code_opaque() {
        let input = "Use `<telegram-message from=\"assistant\" to=\"user\">` to wrap replies.";
        let doc = parse(input);
        assert!(!doc.nodes.iter().any(|n| matches!(n, Node::Message(_))));
    }

    #[test]
    fn test_round_trip_message() {
        let input = "<telegram-message from=\"assistant\" to=\"user\" id=\"73\">Hello!</telegram-message>";
        let doc = parse(input);
        let output = serialize(&doc);
        let doc2 = parse(&output);
        assert_eq!(doc, doc2);
    }

    #[test]
    fn test_round_trip_reaction() {
        let input = "<telegram-reaction from=\"assistant\" action=\"add\" emoji=\"👍\" message-id=\"42\" />";
        let doc = parse(input);
        let output = serialize(&doc);
        let doc2 = parse(&output);
        assert_eq!(doc, doc2);
    }

    #[test]
    fn test_malformed_tag_becomes_text() {
        let input = "<telegram-message from=\"assistant\"";
        let doc = parse(input);
        assert_eq!(doc.nodes.len(), 1);
        assert!(matches!(&doc.nodes[0], Node::Text(_)));
    }

    #[test]
    fn test_mixed_nodes() {
        let input = "Let me think.\n<telegram-message from=\"assistant\" to=\"user\">Answer</telegram-message>\n<telegram-reaction from=\"assistant\" action=\"add\" emoji=\"👍\" message-id=\"1\" />";
        let doc = parse(input);
        assert_eq!(doc.nodes.len(), 4);
        assert!(matches!(&doc.nodes[0], Node::Text(_)));
        assert!(matches!(&doc.nodes[1], Node::Message(_)));
        assert!(matches!(&doc.nodes[2], Node::Text(t) if t == "\n"));
        assert!(matches!(&doc.nodes[3], Node::Reaction(_)));
    }

    #[test]
    fn test_user_tags_parsed() {
        let input = "<telegram-message from=\"user\" to=\"assistant\" id=\"5\">Hello</telegram-message>";
        let doc = parse(input);
        assert_eq!(doc.nodes.len(), 1);
        let Node::Message(el) = &doc.nodes[0] else { panic!("expected Message") };
        assert_eq!(el.from, "user");
    }
}
