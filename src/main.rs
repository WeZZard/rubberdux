use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting rubberdux...");

    let bot = Bot::from_env();

    teloxide::repl(bot, |bot: Bot, msg: Message| async move {
        if let Some(text) = msg.text() {
            log::info!("Received: {}", text);
            bot.send_message(msg.chat.id, format!("Echo: {}", text))
                .await?;
        }
        Ok(())
    })
    .await;
}
