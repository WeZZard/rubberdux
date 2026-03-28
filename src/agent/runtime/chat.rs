use async_openai::config::OpenAIConfig;
use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs,
};
use async_openai::Client;
use tokio::sync::mpsc;

use crate::channel::{AgentResponse, UserMessage};

pub async fn run(mut rx: mpsc::Receiver<UserMessage>, system_prompt: String) {
    let config = OpenAIConfig::new()
        .with_api_base(
            std::env::var("OPENAI_API_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        )
        .with_api_key(std::env::var("OPENAI_API_KEY").unwrap_or_default());

    let client = Client::with_config(config);
    let model =
        std::env::var("RUBBERDUX_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());

    log::info!("Agent loop started (model: {})", model);

    while let Some(msg) = rx.recv().await {
        let result = process_message(&client, &model, &system_prompt, &msg.text).await;

        let response = match result {
            Ok(text) => AgentResponse { text },
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

async fn process_message(
    client: &Client<OpenAIConfig>,
    model: &str,
    system_prompt: &str,
    user_text: &str,
) -> Result<String, async_openai::error::OpenAIError> {
    let system_msg = ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(system_prompt)
            .build()?,
    );

    let user_msg = ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(user_text)
            .build()?,
    );

    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(vec![system_msg, user_msg])
        .build()?;

    let response = client.chat().create(request).await?;

    let text = response
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_else(|| "(empty response)".into());

    Ok(text)
}
