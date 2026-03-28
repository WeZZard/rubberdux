/// Parsed segments from model output.
#[derive(Debug, PartialEq)]
pub enum Segment {
    /// Text content to send to the user via Telegram.
    TelegramMessage { content: String },
    /// Reaction to add to a user message.
    TelegramReaction { emoji: String, message_id: i32 },
    /// Internal reasoning or other text (not sent to user).
    Internal(String),
}

/// Parses model output into segments, respecting code blocks.
///
/// Rules:
/// - `<telegram-message from="assistant" ...>content</telegram-message>` → TelegramMessage
/// - `<telegram-reaction from="assistant" ... />` → TelegramReaction
/// - Content inside ``` or ` code spans is not parsed for tags
/// - Everything else is Internal
pub fn parse_model_output(input: &str) -> Vec<Segment> {
    use super::markup::{self, Node};

    let doc = markup::parse(input);
    let mut segments = Vec::new();
    let mut internal_buf = String::new();

    for node in doc.nodes {
        match &node {
            Node::Message(el) if el.from == "assistant" => {
                flush_internal(&mut internal_buf, &mut segments);
                segments.push(Segment::TelegramMessage {
                    content: el.content.clone(),
                });
            }
            Node::Reaction(el) if el.from == "assistant" => {
                flush_internal(&mut internal_buf, &mut segments);
                if let Ok(message_id) = el.message_id.parse::<i32>() {
                    segments.push(Segment::TelegramReaction {
                        emoji: el.emoji.clone(),
                        message_id,
                    });
                } else {
                    internal_buf.push_str(&markup::serialize_node(&node));
                }
            }
            _ => {
                internal_buf.push_str(&markup::serialize_node(&node));
            }
        }
    }

    flush_internal(&mut internal_buf, &mut segments);
    segments
}

fn flush_internal(buf: &mut String, segments: &mut Vec<Segment>) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        segments.push(Segment::Internal(trimmed.to_owned()));
    }
    buf.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_is_internal() {
        let segments = parse_model_output("Just thinking here.");
        assert_eq!(segments, vec![Segment::Internal("Just thinking here.".into())]);
    }

    #[test]
    fn test_telegram_message_extracted() {
        let input = "<telegram-message from=\"assistant\" to=\"user\">Hello!</telegram-message>";
        let segments = parse_model_output(input);
        assert_eq!(segments, vec![Segment::TelegramMessage { content: "Hello!".into() }]);
    }

    #[test]
    fn test_telegram_message_with_id_attribute() {
        let input = "<telegram-message from=\"assistant\" to=\"user\" id=\"73\">Hello!</telegram-message>";
        let segments = parse_model_output(input);
        assert_eq!(segments, vec![Segment::TelegramMessage { content: "Hello!".into() }]);
    }

    #[test]
    fn test_telegram_message_with_surrounding_reasoning() {
        let input = "Let me think.\n<telegram-message from=\"assistant\" to=\"user\">The answer is 42.</telegram-message>\nDone reasoning.";
        let segments = parse_model_output(input);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0], Segment::Internal("Let me think.".into()));
        assert_eq!(segments[1], Segment::TelegramMessage { content: "The answer is 42.".into() });
        assert_eq!(segments[2], Segment::Internal("Done reasoning.".into()));
    }

    #[test]
    fn test_telegram_reaction_parsed() {
        let input = "<telegram-reaction from=\"assistant\" action=\"add\" emoji=\"👍\" message-id=\"42\" />";
        let segments = parse_model_output(input);
        assert_eq!(segments, vec![Segment::TelegramReaction { emoji: "👍".into(), message_id: 42 }]);
    }

    #[test]
    fn test_message_and_reaction_together() {
        let input = "<telegram-reaction from=\"assistant\" action=\"add\" emoji=\"❤️\" message-id=\"10\" />\n<telegram-message from=\"assistant\" to=\"user\">Great question!</telegram-message>";
        let segments = parse_model_output(input);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], Segment::TelegramReaction { emoji: "❤️".into(), message_id: 10 });
        assert_eq!(segments[1], Segment::TelegramMessage { content: "Great question!".into() });
    }

    #[test]
    fn test_code_block_not_parsed() {
        let input = "```xml\n<telegram-message from=\"assistant\" to=\"user\">Not a real tag</telegram-message>\n```";
        let segments = parse_model_output(input);
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Internal(_)));
        if let Segment::Internal(text) = &segments[0] {
            assert!(text.contains("<telegram-message"), "Code block content should be preserved as-is");
        }
    }

    #[test]
    fn test_inline_code_not_parsed() {
        let input = "Use `<telegram-message from=\"assistant\" to=\"user\">` to wrap replies.";
        let segments = parse_model_output(input);
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Internal(_)));
    }

    #[test]
    fn test_mixed_code_block_and_real_tag() {
        let input = "Here's an example:\n```\n<telegram-message from=\"assistant\" to=\"user\">fake</telegram-message>\n```\n<telegram-message from=\"assistant\" to=\"user\">Real reply here.</telegram-message>";
        let segments = parse_model_output(input);

        let messages: Vec<&Segment> = segments.iter()
            .filter(|s| matches!(s, Segment::TelegramMessage { .. }))
            .collect();

        assert_eq!(messages.len(), 1, "Only the real tag outside code block should be parsed");
        assert_eq!(messages[0], &Segment::TelegramMessage { content: "Real reply here.".into() });
    }

    #[test]
    fn test_user_tags_ignored() {
        let input = "<telegram-message from=\"user\" to=\"assistant\" id=\"5\">Hello</telegram-message>";
        let segments = parse_model_output(input);
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Internal(_)));
    }

    #[test]
    fn test_multiline_content_preserved() {
        let input = "<telegram-message from=\"assistant\" to=\"user\">Line 1\nLine 2\n**Bold**</telegram-message>";
        let segments = parse_model_output(input);
        assert_eq!(segments, vec![Segment::TelegramMessage {
            content: "Line 1\nLine 2\n**Bold**".into()
        }]);
    }

    #[test]
    fn test_empty_input() {
        let segments = parse_model_output("");
        assert!(segments.is_empty());
    }

    #[test]
    fn test_reaction_without_assistant_ignored() {
        let input = "<telegram-reaction from=\"user\" action=\"add\" emoji=\"👍\" message-id=\"1\" />";
        let segments = parse_model_output(input);
        assert!(segments.iter().all(|s| !matches!(s, Segment::TelegramReaction { .. })));
    }
}
