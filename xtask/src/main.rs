use clap::{Parser, Subcommand};

mod provision;
mod launch;
mod stop;
mod sessions;

use provision::provision_images;
use launch::launch_rubberdux;
use stop::stop_rubberdux;
use sessions::{list_sessions, archive_session, delete_session, clear_sessions};

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
    /// Provision VMs, build, and launch rubberdux
    Launch,
    /// Stop running rubberdux process and VMs
    Stop,
    /// Manage sessions
    Sessions {
        #[command(subcommand)]
        action: SessionCommands,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    /// List all sessions
    List,
    /// Archive a session
    Archive {
        session_id: String,
    },
    /// Delete a session
    Delete {
        session_id: String,
    },
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
        Commands::Launch => {
            if let Err(e) = launch_rubberdux().await {
                eprintln!("Launch failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Stop => {
            if let Err(e) = stop_rubberdux().await {
                eprintln!("Stop failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Sessions { action } => {
            match action {
                SessionCommands::List => list_sessions(),
                SessionCommands::Archive { session_id } => archive_session(&session_id),
                SessionCommands::Delete { session_id } => delete_session(&session_id),
                SessionCommands::Clear => clear_sessions(),
            }
        }
    }
}
