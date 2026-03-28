mod agent;
mod channel;
mod error;
mod markdown;
mod prompt;
mod provider;

use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    log::info!("Starting rubberdux...");

    let prompt_dir = prompt::prompt_dir();
    let system_prompt = prompt::load_system_prompt(&prompt_dir).unwrap_or_else(|e| {
        log::error!("Failed to load system prompt from {:?}: {}", prompt_dir, e);
        std::process::exit(1);
    });

    log::info!("Loaded system prompt from {:?}", prompt_dir);

    let client = provider::moonshot::MoonshotClient::from_env();
    log::info!("Moonshot client initialized (model: {})", client.model());

    let (tx, rx) = tokio::sync::mpsc::channel(32);

    tokio::spawn(agent::runtime::chat::run(rx, client, system_prompt));

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| {
        log::error!("TELEGRAM_BOT_TOKEN is not set");
        std::process::exit(1);
    });
    let bot = Bot::new(bot_token);
    channel::adapter::telegram::run(bot, tx).await;
}
