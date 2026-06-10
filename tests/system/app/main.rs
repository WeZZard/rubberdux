//! System tests for the subprocess-backed App runtime (`LocalSupervisor` +
//! the real `rubberduxd --agent` worker). These exercise the complete native
//! stack: a real OS subprocess per App, the RPC duplex, crash isolation and
//! auto-restart, idle tombstoning with on-disk history restore, and two-App
//! peer messaging with a tombstoned target. Per the mock-data policy (root
//! `CLAUDE.md`), system tests use the real model — so every case here is the
//! human-run live suite and is gated on live-LLM credentials.
//!
//! This is a `harness = false` target for one reason: `LocalSupervisor` spawns
//! `std::env::current_exe() --agent …` for each worker. Under a test, the
//! current executable is *this* binary, so `main` must re-enter worker mode
//! when invoked with `--agent`, delegating to the same
//! `rubberdux::app::runtime::worker::run_app_worker` the production binary
//! uses. Without that delegation the spawned child would try to run the test
//! runner instead of the worker. See `docs/app/runtime/worker-lifecycle.md`.
//!
//! Run the live suite with credentials + a host that can spawn subprocesses:
//!
//! ```bash
//! RUBBERDUX_LLM_API_KEY=… RUBBERDUX_LLM_BASE_URL=… RUBBERDUX_LLM_MODEL=… \
//!   cargo test --test app_system
//! ```

#[path = "../../support/live_gate.rs"]
mod live_gate;

mod idle_tombstone_restore;
mod interaction_round_trip;
mod peer_send_tombstoned_target;
mod subprocess_crash_restart;
mod support;

/// Entry point. When invoked as a worker (the `LocalSupervisor` child command
/// shape), delegate to the production worker so the spawned process behaves
/// exactly like a real `rubberduxd --agent`. Otherwise run the gated system
/// cases sequentially and report a pass/skip/fail summary, exiting non-zero on
/// any failure so the suite fails the build when run live.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--agent") {
        run_as_worker(&args);
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    /// A boxed future for one gated system case, so the cases can be stored and
    /// run uniformly in sequence.
    type CaseFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

    let cases: &[(&str, fn() -> CaseFuture)] = &[
        ("subprocess_crash_restart", || {
            Box::pin(subprocess_crash_restart::run())
        }),
        ("idle_tombstone_restore", || {
            Box::pin(idle_tombstone_restore::run())
        }),
        ("peer_send_tombstoned_target", || {
            Box::pin(peer_send_tombstoned_target::run())
        }),
        ("interaction_round_trip", || {
            Box::pin(interaction_round_trip::run())
        }),
    ];

    let mut failed = false;
    for (name, case) in cases {
        eprintln!("RUN  {name}");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(case());
        }));
        match result {
            Ok(()) => eprintln!("PASS {name}"),
            Err(_) => {
                eprintln!("FAIL {name}");
                failed = true;
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
}

/// Re-enter worker mode using the same arguments `LocalSupervisor` passes to its
/// child, delegating to the production worker. Mirrors the binary's
/// `RunMode::AppWorker` dispatch in `src/main.rs`.
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
