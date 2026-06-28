use clap::{Parser, Subcommand};

mod app;
mod bootstrap;
mod build;
mod distribute;
mod git_hooks;
mod launch;
mod lint;
mod provision;
mod replay;
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
    /// Lint the design-documentation structure (runs in the git pre-commit hook)
    Lint,
    /// Arm the git pre-commit linter for this clone (sets core.hooksPath)
    InstallGitHooks,
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
    /// List, restore, fork, and replay recorded agent sessions
    Replay {
        #[command(subcommand)]
        action: ReplayAction,
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
    /// Build the Rust backend + run the macOS app's XCTest target
    Test,
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

#[derive(Subcommand)]
enum ReplayAction {
    /// List sessions with their snapshots (hot/cold) and branches/pins
    List {
        /// Restrict output to a single session ID
        #[arg(long)]
        session: Option<String>,
    },
    /// Restore a session to a tick and print the resulting tick + World digest
    Restore {
        /// The session ID to restore
        #[arg(long)]
        session: String,
        /// The tick to restore to (defaults to the latest recorded tick)
        #[arg(long)]
        at: Option<u64>,
    },
    /// Fork a session at a tick with an exogenous edit, printing the BranchId
    Fork {
        /// The session ID to fork
        #[arg(long)]
        session: String,
        /// The tick to fork at (the edit's target tick)
        #[arg(long)]
        at: u64,
        /// The exogenous edit, as `<kind>:<text>` (e.g. `user:hello`)
        #[arg(long)]
        edit: String,
    },
    /// Replay a branch to quiescence (replay → live; LIVE, paid)
    Run {
        /// The session the branch belongs to
        #[arg(long)]
        session: String,
        /// The branch ID to run
        branch: String,
    },
    /// Diff two branch Worlds, printing the divergence tick + summary
    Diff {
        /// The session the branches belong to
        #[arg(long)]
        session: String,
        /// The first operand: a branch ID, or `main`/`original`
        branch_a: String,
        /// The second operand: a branch ID, or `main`/`original`
        branch_b: String,
    },
    /// Prune a session: snapshot retention + cold-tier + dead-branch GC,
    /// printing the evicted/tiered/reclaimed/protected counts
    Prune {
        /// The session ID to prune (defaults to the latest session)
        #[arg(long)]
        session: Option<String>,
        /// Number of latest snapshots to keep (defaults to the retention cap, 3)
        #[arg(long)]
        keep: Option<u32>,
    },
    /// Drop a branch (tombstone it) so the next prune reclaims it
    Drop {
        /// The session the branch belongs to
        #[arg(long)]
        session: String,
        /// The branch ID to drop
        branch: String,
    },
    /// Pin a branch as a reachability root so prune never reclaims it
    Pin {
        /// The session the branch belongs to
        #[arg(long)]
        session: String,
        /// The branch ID to pin
        branch: String,
    },
    /// Unpin a branch, clearing its reachability-root flag
    Unpin {
        /// The session the branch belongs to
        #[arg(long)]
        session: String,
        /// The branch ID to unpin
        branch: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Opportunistically arm the pre-commit linter for contributors who build,
    // without re-arming on the install command itself. Best-effort and silent.
    if !matches!(cli.command, Commands::InstallGitHooks) {
        git_hooks::ensure_armed_quietly();
    }
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
        Commands::Lint => {
            if let Err(e) = lint::lint() {
                eprintln!("Lint failed: {}", e);
                std::process::exit(1);
            }
        }
        Commands::InstallGitHooks => {
            if let Err(e) = git_hooks::install() {
                eprintln!("Install git hooks failed: {}", e);
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
            AppCommands::Test => {
                if let Err(e) = app::test() {
                    eprintln!("App test failed: {}", e);
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
        Commands::Replay { action } => match action {
            ReplayAction::List { session } => {
                if let Err(e) = replay::list(session.as_deref()) {
                    eprintln!("Replay list failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Restore { session, at } => {
                if let Err(e) = replay::restore(&session, at) {
                    eprintln!("Replay restore failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Fork { session, at, edit } => {
                if let Err(e) = replay::fork(&session, at, &edit) {
                    eprintln!("Replay fork failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Run { session, branch } => {
                if let Err(e) = replay::run(&session, &branch).await {
                    eprintln!("Replay run failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Diff {
                session,
                branch_a,
                branch_b,
            } => {
                if let Err(e) = replay::diff(&session, &branch_a, &branch_b) {
                    eprintln!("Replay diff failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Prune { session, keep } => {
                if let Err(e) = replay::prune(session.as_deref(), keep) {
                    eprintln!("Replay prune failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Drop { session, branch } => {
                if let Err(e) = replay::drop_branch(&session, &branch) {
                    eprintln!("Replay drop failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Pin { session, branch } => {
                if let Err(e) = replay::pin(&session, &branch) {
                    eprintln!("Replay pin failed: {}", e);
                    std::process::exit(1);
                }
            }
            ReplayAction::Unpin { session, branch } => {
                if let Err(e) = replay::unpin(&session, &branch) {
                    eprintln!("Replay unpin failed: {}", e);
                    std::process::exit(1);
                }
            }
        },
    }
}
