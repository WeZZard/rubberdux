use crate::llm::{ChatMessage, LlmClient, build_request_json, parse_response_text};

/// Render guidance comments into a natural user message using an LLM.
pub async fn render_guidance<C: LlmClient>(guidance: &[String], client: &C, model: &str) -> String {
    if guidance.is_empty() {
        return String::new();
    }

    let prompt = guidance.join("\n");

    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: "You are a test user generating natural messages for an AI assistant. \
                      Produce a concise, realistic user message based on the guidance. \
                      Output only the message text, no explanations."
                .into(),
        },
        ChatMessage {
            role: "user".into(),
            content: prompt,
        },
    ];

    let body = build_request_json(model, &messages, 0.7);
    match client.chat_raw(body).await {
        Ok(raw) => match parse_response_text(&raw) {
            Ok(text) => text.trim().to_string(),
            Err(e) => {
                eprintln!(
                    "Warning: Guidance rendering parse failed: {}. Using raw guidance.",
                    e
                );
                guidance.first().cloned().unwrap_or_default()
            }
        },
        Err(e) => {
            eprintln!(
                "Warning: Guidance rendering failed: {}. Using raw guidance.",
                e
            );
            guidance.first().cloned().unwrap_or_default()
        }
    }
}
