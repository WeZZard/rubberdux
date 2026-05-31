use clap::{Parser, Subcommand};

mod app;
mod bootstrap;
mod build;
mod distribute;
mod launch;
mod provision;
mod sessions;
mod stop;

use bootstrap::bootstrap;
use build::build_daemon;
use distribute::distribute as distribute_app;
use launch::launch_rubberdux;
use provision::provision_images;
use sessions::{archive_session, clear_sessions, delete_session, list_sessions};
use stop::stop_rubberdux;

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Provision VM images (check hashes, rebuild if stale)
    Provision {
        /// Specific image to provision (ubuntu24, macos15, macos26)
        image: Option<String>,
    },
    /// Build rubberdux daemon with versioned binary management
    Build,
    /// Provision VMs and launch rubberdux (no build)
    Launch,
    /// Bootstrap: build, launch, health-check, rollback on failure
    Bootstrap,
    /// Stop running rubberdux process and VMs
    Stop,
    /// Build or run the macOS app (builds Rust backend first)
    App {
        #[command(subcommand)]
        action: AppCommands,
    },
    /// Build and package the macOS app for distribution
    Distribute,
    /// Manage sessions
    Sessions {
        #[command(subcommand)]
        action: SessionCommands,
    },
}

#[derive(Subcommand)]
enum AppCommands {
    /// Build Rust backend + macOS app
    Build {
        /// Build in release mode
        #[arg(long)]
        release: bool,
    },
    /// Build and launch the macOS app
    Run {
        /// Build in release mode
        #[arg(long)]
        release: bool,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    /// List all sessions
    List,
    /// Archive a session
    Archive { session_id: String },
    /// Delete a session
    Delete { session_id: String },
    /// Clear all sessions except latest
    Clear,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Provision { image } => {
            if let Err(e) = provision_images(image).await {
                eprintln!("Provision failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Build => {
            if let Err(e) = build_daemon().await {
                eprintln!("Build failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Launch => {
            if let Err(e) = launch_rubberdux().await {
                eprintln!("Launch failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Bootstrap => {
            if let Err(e) = bootstrap().await {
                eprintln!("Bootstrap failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Stop => {
            if let Err(e) = stop_rubberdux().await {
                eprintln!("Stop failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::App { action } => match action {
            AppCommands::Build { release } => {
                if let Err(e) = app::build(release) {
                    eprintln!("App build failed: {}", e);
                    std::process::exit(1);
                }
            }
            AppCommands::Run { release } => {
                if let Err(e) = app::run(release) {
                    eprintln!("App run failed: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Commands::Distribute => {
            if let Err(e) = distribute_app().await {
                eprintln!("Distribute failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Sessions { action } => match action {
            SessionCommands::List => list_sessions(),
            SessionCommands::Archive { session_id } => archive_session(&session_id),
            SessionCommands::Delete { session_id } => delete_session(&session_id),
            SessionCommands::Clear => clear_sessions(),
        },
    }
}
