use tokio::sync::mpsc;

use crate::channel::{AgentResponse, UserMessage};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};

const DEFAULT_BEST_PERFORMANCE_TOKENS: usize = 153_600;

pub async fn run(
    mut rx: mpsc::Receiver<UserMessage>,
    client: MoonshotClient,
    system_prompt: String,
) {
    let best_perf_tokens: usize = std::env::var("RUBBERDUX_LLM_BEST_PERFORMANCE_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BEST_PERFORMANCE_TOKENS);

    log::info!(
        "Agent loop started (model: {}, best_performance_tokens: {})",
        client.model(),
        best_perf_tokens
    );

    let mut history: Vec<Message> = Vec::new();
    let mut last_prompt_tokens: usize = 0;

    while let Some(msg) = rx.recv().await {
        let interpreted = &msg.interpreted;
        let text_preview = &interpreted.text;
        let has_attachments = !interpreted.attachments.is_empty();

        log::info!(
            "Processing message: {} (attachments: {})",
            text_preview,
            interpreted.attachments.len()
        );

        // Evict oldest pairs if approaching the best performance threshold
        let estimated_new = text_preview.len() / 4;
        while last_prompt_tokens + estimated_new > best_perf_tokens
            && evict_oldest_pair(&mut history)
        {
            log::info!(
                "Evicting oldest message pair (estimated context: {} + {} = {}, threshold: {})",
                last_prompt_tokens,
                estimated_new,
                last_prompt_tokens + estimated_new,
                best_perf_tokens
            );
            last_prompt_tokens = last_prompt_tokens.saturating_sub(last_prompt_tokens / 10);
        }

        let messages = client.build_messages(&system_prompt, &history, interpreted);

        let result = client.chat(messages).await;
        log::info!("LLM responded for: {}", text_preview);

        let response = match result {
            Ok(chat_response) => {
                last_prompt_tokens = chat_response.usage.prompt_tokens;
                log::debug!(
                    "Token usage: prompt={}, completion={}, total={}, cached={}",
                    chat_response.usage.prompt_tokens,
                    chat_response.usage.completion_tokens,
                    chat_response.usage.total_tokens,
                    chat_response.usage.cached_tokens,
                );

                let response_text = chat_response
                    .choices
                    .first()
                    .map(|c| c.message.content_text())
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_owned())
                    .unwrap_or_else(|| "(empty response)".into());

                // Store text-only in history (no base64 data — too large)
                let history_text = if has_attachments {
                    format!("{} [with {} attachment(s)]", text_preview, interpreted.attachments.len())
                } else {
                    text_preview.clone()
                };

                history.push(Message::User {
                    content: UserContent::Text(history_text),
                });
                history.push(Message::Assistant {
                    content: Some(response_text.clone()),
                    tool_calls: None,
                    partial: None,
                });

                AgentResponse {
                    text: response_text,
                }
            }
            Err(e) => {
                log::error!("LLM call failed: {}", e);
                AgentResponse {
                    text: format!("Sorry, I encountered an error: {}", e),
                }
            }
        };

        let _ = msg.reply_tx.send(response);
    }

    log::info!("Agent loop shutting down");
}

fn evict_oldest_pair(history: &mut Vec<Message>) -> bool {
    if history.len() >= 2 {
        history.remove(0);
        history.remove(0);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eviction_removes_oldest_pairs() {
        let mut history = vec![
            Message::User { content: UserContent::Text("old1".into()) },
            Message::Assistant { content: Some("reply1".into()), tool_calls: None, partial: None },
            Message::User { content: UserContent::Text("old2".into()) },
            Message::Assistant { content: Some("reply2".into()), tool_calls: None, partial: None },
            Message::User { content: UserContent::Text("recent".into()) },
            Message::Assistant { content: Some("reply3".into()), tool_calls: None, partial: None },
        ];

        assert!(evict_oldest_pair(&mut history));
        assert_eq!(history.len(), 4);

        assert!(matches!(&history[2], Message::User { content: UserContent::Text(t) } if t == "recent"));
        assert!(matches!(&history[3], Message::Assistant { content: Some(c), .. } if c == "reply3"));
    }

    #[test]
    fn test_no_eviction_under_threshold() {
        let history = vec![
            Message::User { content: UserContent::Text("hello".into()) },
            Message::Assistant { content: Some("hi".into()), tool_calls: None, partial: None },
        ];

        let last_prompt_tokens: usize = 100;
        let estimated_new: usize = 10;
        let best_perf_tokens: usize = 153_600;

        assert!(last_prompt_tokens + estimated_new <= best_perf_tokens);
        assert_eq!(history.len(), 2);
    }
}
