use clap::{Parser, Subcommand};

mod provision;
mod launch;

use provision::provision_images;
use launch::launch_rubberdux;

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
    }
}
