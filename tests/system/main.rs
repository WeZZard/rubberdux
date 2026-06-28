//! System test for the full-stack live UI-manipulation loop (VC-E.1).
//!
//! This is the `system` `harness = false` target. It exercises the complete
//! native stack for the **agent-drives-real-UI** path (Story 3 / Theme 2):
//! the daemon's [`LocalSupervisor`] spawns a real `rubberduxd --agent` worker
//! that drives the ECS World, the host's [`SurfaceRouter`] relays surface frames,
//! and (once the backend produces a drive) the macOS surface client applies the
//! agent's `set_value` to a real Accessibility element — verified out-of-band via
//! cua-driver. Per the mock-data policy (root `CLAUDE.md`), it uses the real model
//! and is gated on live-LLM credentials.
//!
//! `harness = false` for the same reason as `app_system` (`tests/system/app`):
//! [`LocalSupervisor`] spawns `std::env::current_exe() --agent …` for each worker,
//! so under a test the current executable is *this* binary and `main` must
//! re-enter worker mode when invoked with `--agent`, delegating to the same
//! `rubberdux::app::runtime::worker::run_app_worker` the production binary uses.
//! See `docs/app/runtime/worker-lifecycle.md` and `docs/agent/world/ecs-runtime.md`.
//!
//! Run the live case from the repo root:
//!
//! ```bash
//! cargo test --test system app::surface_drive -- --nocapture
//! ```

#[path = "../support/live_gate.rs"]
mod live_gate;

mod app {
    #[path = "counterfactual_branch_live.rs"]
    pub mod counterfactual_branch_live;
    #[path = "endurance_blob_loopback.rs"]
    pub mod endurance_blob_loopback;
    #[path = "endurance_compaction_loopback.rs"]
    pub mod endurance_compaction_loopback;
    #[path = "endurance_overrides_loopback.rs"]
    pub mod endurance_overrides_loopback;
    #[path = "peer_drive_loopback.rs"]
    pub mod peer_drive_loopback;
    #[path = "session_resume.rs"]
    pub mod session_resume;
    #[path = "surface_drive.rs"]
    pub mod surface_drive;
    #[path = "surface_mixed_replay.rs"]
    pub mod surface_mixed_replay;
    #[path = "surface_modes.rs"]
    pub mod surface_modes;
    #[path = "surface_support.rs"]
    pub mod surface_support;
}

/// Entry point. When invoked as a worker (the `LocalSupervisor` child command
/// shape), delegate to the production worker so the spawned process behaves
/// exactly like a real `rubberduxd --agent`. Otherwise run the selected gated
/// case: the VC-E.2 three-modes projection case (`app::surface_modes`) when the
/// invocation selects it — `RUBBERDUX_SYSTEM_E2E_CASE=modes` (set by the e2e
/// runner `tests/e2e/aarch64-apple-macos/test_modes_projection.sh`) or a
/// `surface_modes` test filter — the VC-E.3 record→replay capstone
/// (`app::surface_mixed_replay`) when `RUBBERDUX_SYSTEM_E2E_CASE=mixed_replay` (set
/// by `tests/e2e/aarch64-apple-macos/test_mixed_mode_replay.sh`) or a
/// `surface_mixed_replay` test filter — otherwise the default VC-E.1 case
/// (`app::surface_drive`), so the audited V-system-drive invocation
/// `cargo test --test system app::surface_drive` keeps running surface_drive.
fn main() {
    // The test binary's `main` is NOT `src/main.rs`, so the production binary's
    // `dotenvy::dotenv()` never runs here. Load the repo-root `.env` ourselves so
    // a run "from the repo root" picks up RUBBERDUX_LLM_* exactly as the daemon
    // does, and the worker subprocess (which inherits this process's env) sees
    // the same credentials. No `dotenvy` dependency is available to a test target.
    load_dotenv_if_present();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--agent") {
        run_as_worker(&args);
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    // Select the case the invocation asks for (the e2e runners set
    // `RUBBERDUX_SYSTEM_E2E_CASE`; a matching test filter selects it too). Default
    // stays VC-E.1 `surface_drive`.
    let case = std::env::var("RUBBERDUX_SYSTEM_E2E_CASE").unwrap_or_default();
    let want_modes = case == "modes" || args.iter().any(|a| a.contains("surface_modes"));
    let want_mixed_replay =
        case == "mixed_replay" || args.iter().any(|a| a.contains("surface_mixed_replay"));
    // The VC-1.2 counterfactual-replay LIVE half (`app::counterfactual_branch_live`):
    // selected by `RUBBERDUX_SYSTEM_E2E_CASE=counterfactual_branch_live` or a matching
    // test filter. It needs no surface/macOS harness — a pure CallModel branch flip.
    let want_counterfactual = case == "counterfactual_branch_live"
        || args.iter().any(|a| a.contains("counterfactual_branch_live"));
    // The VC-4.1 session-resume case (`app::session_resume`): selected by
    // `RUBBERDUX_SYSTEM_E2E_CASE=session_resume` or a matching test filter. Like
    // the counterfactual case it needs no surface/macOS harness — it drives the
    // worker's resume path (`WorldDriver::open` over the latest session) in-process.
    let want_session_resume =
        case == "session_resume" || args.iter().any(|a| a.contains("session_resume"));
    // The VC-3.2 cross-World peer-drive LIVE loopback (`app::peer_drive_loopback`):
    // selected by `RUBBERDUX_SYSTEM_E2E_CASE=peer_drive_loopback` or a matching test
    // filter. Two local Worlds over the real broker — no surface/macOS harness, no VM.
    let want_peer_drive_loopback = case == "peer_drive_loopback"
        || args.iter().any(|a| a.contains("peer_drive_loopback"));
    // The VC-3.2 per-entity model-override LIVE loopback
    // (`app::endurance_overrides_loopback`): selected by
    // `RUBBERDUX_SYSTEM_E2E_CASE=endurance_overrides_loopback` or a matching test
    // filter. A sub-agent with a model override makes a real CallModel against the
    // override — no surface/macOS harness, no VM.
    let want_endurance_overrides = case == "endurance_overrides_loopback"
        || args.iter().any(|a| a.contains("endurance_overrides_loopback"));
    // The VC-1.4 blob externalize+retrieve LIVE loopback
    // (`app::endurance_blob_loopback`): selected by
    // `RUBBERDUX_SYSTEM_E2E_CASE=endurance_blob_loopback` or a matching test filter.
    // A turn carrying a real over-cap payload externalizes it to the BlobStore and
    // retrieves it by hash against a real model — no surface/macOS harness, no VM.
    let want_endurance_blob = case == "endurance_blob_loopback"
        || args.iter().any(|a| a.contains("endurance_blob_loopback"));
    // The VC-2.2 compaction-resume LIVE loopback
    // (`app::endurance_compaction_loopback`): selected by
    // `RUBBERDUX_SYSTEM_E2E_CASE=endurance_compaction_loopback` or a matching test
    // filter. A real low-`context_limit` turn triggers a real compaction, a simulated
    // crash mid-Compacting strands the session, and `open()` resumes and completes the
    // compaction with a real model call — no surface/macOS harness, no VM.
    let want_endurance_compaction = case == "endurance_compaction_loopback"
        || args.iter().any(|a| a.contains("endurance_compaction_loopback"));

    if want_counterfactual {
        runtime.block_on(app::counterfactual_branch_live::run());
    } else if want_endurance_compaction {
        runtime.block_on(app::endurance_compaction_loopback::run());
    } else if want_endurance_blob {
        runtime.block_on(app::endurance_blob_loopback::run());
    } else if want_endurance_overrides {
        runtime.block_on(app::endurance_overrides_loopback::run());
    } else if want_peer_drive_loopback {
        runtime.block_on(app::peer_drive_loopback::run());
    } else if want_session_resume {
        runtime.block_on(app::session_resume::run());
    } else if want_mixed_replay {
        runtime.block_on(app::surface_mixed_replay::run());
    } else if want_modes {
        runtime.block_on(app::surface_modes::run());
    } else {
        runtime.block_on(app::surface_drive::run());
    }
}

/// Re-enter worker mode using the same arguments `LocalSupervisor` passes to its
/// child, delegating to the production worker. Mirrors `tests/system/app/main.rs`
/// and the binary's `RunMode::AppWorker` dispatch in `src/main.rs`.
fn run_as_worker(args: &[String]) {
    fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].as_str())
    }

    let rpc_host = flag_value(args, "--rpc-host")
        .unwrap_or("127.0.0.1:19384")
        .to_string();
    let app_id = flag_value(args, "--task-id").unwrap_or("unknown").to_string();
    let app_session_dir = flag_value(args, "--app-session-dir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build worker tokio runtime");
    runtime.block_on(rubberdux::app::runtime::worker::run_app_worker(
        rpc_host,
        app_id,
        app_session_dir,
    ));
}

/// Minimal `.env` loader: read `KEY=VALUE` lines from a repo-root `.env` and set
/// each var that is not already present in the environment. Mirrors what
/// `dotenvy::dotenv()` does for the production binary, kept dependency-free so the
/// `system` test target is self-sufficient when run from the repo root. Quotes
/// around a value are stripped; comments (`#…`) and blank lines are ignored.
fn load_dotenv_if_present() {
    let Ok(contents) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        if key.is_empty() || std::env::var(key).is_ok() {
            continue;
        }
        // Edition 2024: `set_var` is `unsafe`. This runs before any worker is
        // spawned and before any threads read these vars, so it is sound here.
        unsafe {
            std::env::set_var(key, value);
        }
    }
}
