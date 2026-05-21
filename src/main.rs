mod agent;
#[cfg(feature = "host")]
mod channel;
mod error;
mod gateway;
mod hardened_prompts;
#[cfg(feature = "host")]
mod host;
mod protocol;
mod provider;
mod session;
mod tool;
mod trajectory;
mod workspace;
mod vm;

#[cfg(feature = "host")]
use teloxide::prelude::*;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    #[cfg(feature = "host")]
    {
        run_host().await;
    }
    #[cfg(not(feature = "host"))]
    {
        log::error!("This binary requires the 'host' feature.");
        std::process::exit(1);
    }
}

/// Host mode: runs AgentLoop locally and bridges Telegram.
#[cfg(feature = "host")]
async fn run_host() {
    log::info!("Starting rubberdux in HOST mode...");

    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| {
        log::error!("TELEGRAM_BOT_TOKEN is not set");
        std::process::exit(1);
    });

    let bot = Bot::new(bot_token);
    let host_config = host::HostConfig::from_env();
    host::run(host_config, bot).await;
}
