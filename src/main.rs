mod agent;
mod app;
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
    /// Native app worker: runs a real `AgentLoop` for a single App and bridges
    /// it to the host over RPC. Selected when `--app-session-dir` is present
    /// alongside `--agent`. See `docs/app/runtime/worker-lifecycle.md`.
    AppWorker {
        rpc_host: String,
        app_id: String,
        app_session_dir: std::path::PathBuf,
    },
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn parse_mode(args: &[String]) -> RunMode {
    if !args.contains(&"--agent".to_string()) {
        return RunMode::Host;
    }

    let rpc_host = flag_value(args, "--rpc-host")
        .unwrap_or("127.0.0.1:19384")
        .to_string();

    // `--app-session-dir` selects the native app-worker mode; `--task-id` then
    // carries the App id (reusing the existing VM-child argument shape).
    if let Some(app_session_dir) = flag_value(args, "--app-session-dir") {
        let app_id = flag_value(args, "--task-id").unwrap_or("unknown").to_string();
        return RunMode::AppWorker {
            rpc_host,
            app_id,
            app_session_dir: std::path::PathBuf::from(app_session_dir),
        };
    }

    let task_id = flag_value(args, "--task-id").unwrap_or("unknown").to_string();

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
        RunMode::AppWorker {
            rpc_host,
            app_id,
            app_session_dir,
        } => {
            child::run_app_child(rpc_host, app_id, app_session_dir).await;
        }
    }
}

/// Host mode: runs AgentLoop locally and bridges Telegram.
#[cfg(feature = "host")]
async fn run_host() {
    log::info!("Starting rubberdux in HOST mode...");

    // The Telegram bridge is optional: without a token the host still serves the
    // gateway and the app board (which is all the desktop GUI needs). Only build
    // the bot when a non-empty token is provided.
    let bot = std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(Bot::new);
    if bot.is_none() {
        log::warn!(
            "TELEGRAM_BOT_TOKEN is not set; starting without the Telegram bridge \
             (the gateway and app board are still served)."
        );
    }

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
    fn test_parse_mode_app_worker() {
        let args: Vec<String> = vec![
            "rubberduxd".into(),
            "--agent".into(),
            "--rpc-host".into(),
            "1.2.3.4:19384".into(),
            "--task-id".into(),
            "2026-06-10-00-00-00-UTC".into(),
            "--app-session-dir".into(),
            "/tmp/apps/app-1".into(),
        ];
        match parse_mode(&args) {
            RunMode::AppWorker {
                rpc_host,
                app_id,
                app_session_dir,
            } => {
                assert_eq!(rpc_host, "1.2.3.4:19384");
                assert_eq!(app_id, "2026-06-10-00-00-00-UTC");
                assert_eq!(app_session_dir, std::path::PathBuf::from("/tmp/apps/app-1"));
            }
            _ => panic!("expected RunMode::AppWorker"),
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
