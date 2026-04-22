mod agent;
#[cfg(feature = "host")]
mod channel;
mod error;
#[cfg(feature = "host")]
mod host;
mod hardened_prompts;
mod protocol;
mod provider;
mod tool;
mod vm;

use unicode_segmentation::UnicodeSegmentation;

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

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(32);
    let (response_tx, mut response_rx) =
        tokio::sync::mpsc::channel::<crate::channel::AgentResponse>(32);

    let bot = Bot::new(bot_token);
    let _bot_for_responses = bot.clone();

    // Spawn response handler: AgentResponse → Telegram
    tokio::spawn(async move {
        while let Some(response) = response_rx.recv().await {
            if response.text.is_empty() {
                continue;
            }
            let preview: String = response.text.graphemes(true).take(100).collect();
            log::info!("Agent response: {}", preview);
        }
    });

    // Run Telegram adapter and host concurrently
    let telegram_task = tokio::spawn(async move {
        channel::adapter::telegram::run(bot, event_tx).await;
    });

    let host_task = tokio::spawn(async move {
        let host_config = host::HostConfig::from_env();
        host::run(host_config, event_rx, response_tx).await;
    });

    tokio::select! {
        _ = telegram_task => log::info!("Telegram adapter stopped"),
        _ = host_task => log::info!("Host stopped"),
    }
}
