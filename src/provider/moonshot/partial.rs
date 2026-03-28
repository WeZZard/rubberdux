use super::Message;

pub fn prefill_message(leading_text: &str) -> Message {
    Message::Assistant {
        content: Some(leading_text.to_owned()),
        reasoning_content: None,
        tool_calls: None,
        partial: Some(true),
    }
}
