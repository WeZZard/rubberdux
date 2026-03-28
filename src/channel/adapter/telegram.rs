use teloxide::prelude::*;
use tokio::sync::mpsc;

use crate::channel::{AgentResponse, UserMessage};

async fn handle_message(
    bot: Bot,
    msg: Message,
    tx: mpsc::Sender<UserMessage>,
) -> Result<(), teloxide::RequestError> {
    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    log::info!("Received: {}", text);

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentResponse>();

    let user_message = UserMessage {
        text: text.to_owned(),
        reply_tx,
    };

    if tx.send(user_message).await.is_err() {
        log::error!("Agent loop channel closed");
        bot.send_message(msg.chat.id, "Sorry, the agent is unavailable.")
            .await?;
        return Ok(());
    }

    match reply_rx.await {
        Ok(response) => {
            bot.send_message(msg.chat.id, &response.text).await?;
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
