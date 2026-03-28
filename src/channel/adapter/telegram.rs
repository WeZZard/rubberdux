use teloxide::prelude::*;
use tokio::sync::mpsc;

use crate::channel::interpreter;
use crate::channel::{AgentResponse, UserMessage};

async fn handle_message(
    bot: Bot,
    msg: Message,
    tx: mpsc::Sender<UserMessage>,
) -> Result<(), teloxide::RequestError> {
    let interpreted = match interpreter::interpret(&bot, &msg).await {
        Some(m) => m,
        None => return Ok(()),
    };

    log::info!(
        "Received: {} (attachments: {})",
        interpreted.text,
        interpreted.attachments.len()
    );

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentResponse>();

    let user_message = UserMessage {
        interpreted,
        reply_tx,
    };

    if tx.send(user_message).await.is_err() {
        log::error!("Agent loop channel closed");
        bot.send_message(msg.chat.id, "Sorry, the agent is unavailable.")
            .await?;
        return Ok(());
    }

    let _ = bot
        .send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await;

    match reply_rx.await {
        Ok(response) => {
            let formatted = crate::markdown::telegram::format(&response.text);
            log::debug!("Raw LLM response:\n{}", response.text);
            log::debug!("Formatted for Telegram:\n{}", formatted);

            let send_result = bot
                .send_message(msg.chat.id, &formatted)
                .parse_mode(teloxide::types::ParseMode::MarkdownV2)
                .await;

            if let Err(e) = send_result {
                log::warn!("MarkdownV2 send failed ({}), retrying without parse_mode", e);
                bot.send_message(msg.chat.id, &response.text).await?;
            }
        }
        Err(_) => {
            log::error!("Agent dropped the reply channel");
            bot.send_message(msg.chat.id, "Sorry, failed to get a response.")
                .await?;
        }
    }

    Ok(())
}

pub async fn run(bot: Bot, tx: mpsc::Sender<UserMessage>) {
    let handler = Update::filter_message().endpoint(handle_message);

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![tx])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
