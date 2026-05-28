mod agent;
#[cfg(feature = "host")]
mod channel;
mod child;
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
mod frontmatter;
mod mindset;
mod workspace;
mod vm;
mod guardrail;

#[cfg(feature = "host")]
use teloxide::prelude::*;

enum RunMode {
    Host,
    Agent { rpc_host: String, task_id: String },
}

fn parse_mode(args: &[String]) -> RunMode {
    if !args.contains(&"--agent".to_string()) {
        return RunMode::Host;
    }

    let rpc_host = args
        .windows(2)
        .find(|w| w[0] == "--rpc-host")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "127.0.0.1:19384".to_string());

    let task_id = args
        .windows(2)
        .find(|w| w[0] == "--task-id")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "unknown".to_string());

    RunMode::Agent { rpc_host, task_id }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args: Vec<String> = std::env::args().collect();

    match parse_mode(&args) {
        RunMode::Host => {
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
        RunMode::Agent { rpc_host, task_id } => {
            child::run_child(rpc_host, task_id).await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mode_default_is_host() {
        let args = &["rubberduxd".into()];
        assert!(matches!(parse_mode(args), RunMode::Host));
    }

    #[test]
    fn test_parse_mode_agent_flag() {
        let args: Vec<String> = vec![
            "rubberduxd".into(),
            "--agent".into(),
            "--rpc-host".into(),
            "1.2.3.4:19384".into(),
            "--task-id".into(),
            "t1".into(),
        ];
        match parse_mode(&args) {
            RunMode::Agent { rpc_host, task_id } => {
                assert_eq!(rpc_host, "1.2.3.4:19384");
                assert_eq!(task_id, "t1");
            }
            _ => panic!("expected RunMode::Agent"),
        }
    }

    #[test]
    fn test_parse_mode_agent_defaults() {
        let args: Vec<String> = vec!["rubberduxd".into(), "--agent".into()];
        match parse_mode(&args) {
            RunMode::Agent { rpc_host, task_id } => {
                assert_eq!(rpc_host, "127.0.0.1:19384");
                assert_eq!(task_id, "unknown");
            }
            _ => panic!("expected RunMode::Agent"),
        }
    }
}
