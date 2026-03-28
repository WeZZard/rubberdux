mod agent;
mod channel;
mod error;
mod prompt;
mod provider;
mod tool;

use std::sync::Arc;

use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    log::info!("Starting rubberdux...");

    let prompt_dir = prompt::prompt_dir();
    let prompt_parts = prompt::load_prompt_parts(&prompt_dir);

    let channel_partial = channel::adapter::telegram::channel_prompt();
    let system_prompt = prompt::compose_system_prompt(&prompt_parts, Some(channel_partial));
    let web_search_prompt = prompt::load_web_search_prompt(&prompt_dir);

    log::info!("Composed system prompt ({} chars)", system_prompt.len());
    log::info!("Loaded web search prompt ({} chars)", web_search_prompt.len());

    let client = Arc::new(provider::moonshot::MoonshotClient::from_env());
    log::info!("Moonshot client initialized (model: {})", client.model());

    let (tx, rx) = tokio::sync::mpsc::channel(32);

    tokio::spawn(agent::runtime::chat::run(rx, client, system_prompt, web_search_prompt));

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| {
        log::error!("TELEGRAM_BOT_TOKEN is not set");
        std::process::exit(1);
    });
    let bot = Bot::new(bot_token);
    channel::adapter::telegram::run(bot, tx).await;
}
