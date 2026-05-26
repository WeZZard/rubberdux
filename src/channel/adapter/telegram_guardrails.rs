use crate::guardrail::{Convention, PostGuardrail, PostGuardrailAction};

pub struct TelegramTagPairGuardrail;

impl PostGuardrail for TelegramTagPairGuardrail {
    fn name(&self) -> &str {
        "telegram_tag_pair"
    }

    fn check(&self, content: &str) -> PostGuardrailAction {
        let has_open = content.contains("<telegram-message");
        let has_close = content.contains("</telegram-message>");

        if !has_open || has_close {
            return PostGuardrailAction::Passed;
        }

        // Attempt repair: append closing tag
        let repaired = format!("{}</telegram-message>", content);
        let doc = crate::channel::adapter::markup::parse(&repaired);
        if doc.nodes.iter().any(|n| {
            matches!(n, crate::channel::adapter::markup::Node::Message(el) if el.from == "assistant")
        }) {
            return PostGuardrailAction::Repaired(repaired);
        }

        PostGuardrailAction::NeedsRetry
    }
}

pub fn convention() -> Convention {
    Convention {
        name: "Telegram Message Format".into(),
        guidance: include_str!("telegram_convention.md").into(),
        pre_guardrails: vec![],
        post_guardrails: vec![Box::new(TelegramTagPairGuardrail)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::PostGuardrail;

    #[test]
    fn test_passed_complete_tags() {
        let g = TelegramTagPairGuardrail;
        let content =
            r#"<telegram-message from="assistant" to="user">Hello</telegram-message>"#;
        assert!(matches!(g.check(content), PostGuardrailAction::Passed));
    }

    #[test]
    fn test_passed_no_tags() {
        let g = TelegramTagPairGuardrail;
        let content = "Just some internal reasoning text.";
        assert!(matches!(g.check(content), PostGuardrailAction::Passed));
    }

    #[test]
    fn test_repaired_missing_close() {
        let g = TelegramTagPairGuardrail;
        let content = r#"<telegram-message from="assistant" to="user">Hello world"#;
        match g.check(content) {
            PostGuardrailAction::Repaired(repaired) => {
                assert!(repaired.ends_with("</telegram-message>"));
                assert!(repaired.contains("Hello world"));
            }
            other => panic!(
                "Expected Repaired, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_repaired_chinese_content() {
        let g = TelegramTagPairGuardrail;
        let content = r#"<telegram-message from="assistant" to="user">你好世界，这是一个很长的中文回复。半导体板块今日暴涨，华为发布韬定律。"#;
        match g.check(content) {
            PostGuardrailAction::Repaired(repaired) => {
                assert!(repaired.ends_with("</telegram-message>"));
                assert!(repaired.contains("半导体"));
            }
            other => panic!(
                "Expected Repaired, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_retry_malformed() {
        let g = TelegramTagPairGuardrail;
        // Has <telegram-message but malformed (no from="assistant") — repair won't
        // produce an assistant Message node
        let content = r#"<telegram-message>broken content without proper attributes"#;
        // After appending </telegram-message>, the parser will try to parse but
        // `from` is required so parse_message_tag returns None, and the content
        // becomes Text
        assert!(matches!(
            g.check(content),
            PostGuardrailAction::NeedsRetry
        ));
    }

    #[test]
    fn test_convention_factory() {
        let conv = convention();
        assert_eq!(conv.name, "Telegram Message Format");
        assert!(!conv.guidance.is_empty());
        assert!(conv.guidance.contains("<telegram-message>"));
        assert!(conv.pre_guardrails.is_empty());
        assert_eq!(conv.post_guardrails.len(), 1);
        assert_eq!(conv.post_guardrails[0].name(), "telegram_tag_pair");
    }
}
