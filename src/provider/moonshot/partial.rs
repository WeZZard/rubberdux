use super::Message;

pub fn prefill_message(leading_text: &str) -> Message {
    Message::Assistant {
        content: Some(leading_text.to_owned()),
        tool_calls: None,
        partial: Some(true),
    }
}
