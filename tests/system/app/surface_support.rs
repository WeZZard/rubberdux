//! Shared full-stack surface harness for the live UI-manipulation system/e2e
//! cases (VC-E.1 `surface_drive` and VC-E.2 `surface_modes`).
//!
//! It boots the SAME native stack `surface_drive.rs` does — a real on-disk App
//! store, the host's [`SurfaceRouter`] + surface-client listener on an ephemeral
//! port, and the subprocess-backed [`LocalSupervisor`] wired to that router — and
//! exposes the boot / macOS-client-launch / event-log-read helpers both cases
//! need. Factoring them here keeps the VC-E.2 modes case from duplicating the
//! proven VC-E.1 boot path while leaving `surface_drive.rs` byte-identical (so the
//! audited V-system-drive case keeps passing). See `surface_drive.rs` for the
//! per-case rationale and `docs/agent/world/ecs-runtime.md`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::broadcast;

use rubberdux::agent::runtime::port::EntryNotification;
use rubberdux::agent::world::inputs::{Event, LogicalInput};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::AppId;
use rubberdux::host::{run_surface_client_listener, SurfaceRouter};

/// The mode look-back window, matching the canonical backend test
/// (`tests/integration/agent/world_surface_mode_replay.rs`) and `surface_drive.rs`:
/// larger than this session's per-edge event count so the origin fold (not the
/// window length) is what is under test.
pub const WINDOW: usize = 16;

/// How long to wait for one turn to settle (a live model call plus, on the tool
/// turn, the continuation). Generous because a real provider round-trip is slow.
pub const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// The single counterpart relationship the live driver routes conversation +
/// human input onto (`HUMAN_EDGE` in `src/agent/world/driver.rs`). Asserted here
/// so a per-edge mode claim names the same edge the runtime stamps.
pub const HUMAN_EDGE: rubberdux::agent::world::world::EdgeId = 0;

/// The DISTINCT app/surface edge the agent's `set_value` `ToolReturned` is stamped
/// onto (`APP_EDGE` in `src/agent/world/driver.rs`), so `mode(APP_EDGE)` folds
/// `Driven` independently of the human conversation edge.
pub const APP_EDGE: rubberdux::agent::world::world::EdgeId = 1;

/// The booted host harness: the App store (to locate a worker's `world-events.jsonl`),
/// the subprocess-backed supervisor (to create Apps + drive turns), the surface
/// port the macOS client must dial, and whether the live macOS/cua-driver half is on.
pub struct Harness {
    pub store: Arc<dyn AppStore>,
    pub supervisor: Arc<LocalSupervisor>,
    pub surface_port: u16,
    pub with_macos: bool,
}

/// Boot the in-process host harness exactly as `surface_drive.rs` does: a real App
/// store (overridable via `RUBBERDUX_SURFACE_DRIVE_HOME` for post-run inspection),
/// the surface router, the surface-client listener on an ephemeral port, and the
/// supervisor wired to the SAME router. `with_macos` is opt-in via
/// `RUBBERDUX_SURFACE_DRIVE_MACOS=1` and a `cua-driver` on PATH.
pub async fn boot() -> Harness {
    let home = std::env::var("RUBBERDUX_SURFACE_DRIVE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            tempfile::tempdir()
                .expect("tempdir for the App store")
                .keep()
        });
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));

    let router = Arc::new(SurfaceRouter::new());
    let surface_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the surface-client listener on an ephemeral port");
    let surface_port = surface_listener
        .local_addr()
        .expect("read the surface listener's local addr")
        .port();
    tokio::spawn(run_surface_client_listener(surface_listener, router.clone()));

    let supervisor = LocalSupervisor::bind_shared(store.clone(), router.clone())
        .await
        .expect("bind the subprocess-backed supervisor with the surface router");

    let with_macos =
        std::env::var("RUBBERDUX_SURFACE_DRIVE_MACOS").is_ok() && cua_driver_on_path();

    Harness {
        store,
        supervisor,
        surface_port,
        with_macos,
    }
}

/// Await a turn's FINAL entry on the App's broadcast stream, returning `true` if
/// one arrives within `within`. A `Lagged` is skipped (the buffer advanced); a
/// closed channel returns `false`. Mirrors `surface_drive.rs::await_final`.
pub async fn await_final(
    rx: &mut broadcast::Receiver<EntryNotification>,
    within: Duration,
) -> bool {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Ok(n) => {
                    if n.is_final {
                        return true;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Read the App's `world-events.jsonl`, retrying briefly so a cross-process flush
/// lag does not race the assertions. Returns the parsed `Event`s in log order.
pub fn read_world_log_settled(app_dir: &Path, within: Duration) -> Vec<Event> {
    let deadline = Instant::now() + within;
    loop {
        let events = read_world_log(app_dir);
        if !events.is_empty() || Instant::now() >= deadline {
            return events;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Parse the App's world event log. Prefers the `latest` session symlink; falls
/// back to the most-recently-modified session log. Lines that do not parse as
/// `Event` are skipped.
pub fn read_world_log(app_dir: &Path) -> Vec<Event> {
    let latest = app_dir.join("latest").join("world-events.jsonl");
    let path = if latest.exists() {
        latest
    } else {
        match newest_session_log(app_dir) {
            Some(p) => p,
            None => return Vec::new(),
        }
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect()
}

/// The newest `sessions/*/world-events.jsonl` under the App dir, by mtime — the
/// fallback when the `latest` symlink is not yet resolvable.
fn newest_session_log(app_dir: &Path) -> Option<PathBuf> {
    let sessions = app_dir.join("sessions");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&sessions).ok()?.flatten() {
        let log = entry.path().join("world-events.jsonl");
        let Ok(meta) = std::fs::metadata(&log) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if newest.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            newest = Some((modified, log));
        }
    }
    newest.map(|(_, p)| p)
}

/// The `LogicalInput` variant name, for the self-explaining diagnostic dumps.
pub fn variant_name(input: &LogicalInput) -> &'static str {
    match input {
        LogicalInput::SessionStarted { .. } => "SessionStarted",
        LogicalInput::UserMessage { .. } => "UserMessage",
        LogicalInput::ModelResponded { .. } => "ModelResponded",
        LogicalInput::ModelFailed { .. } => "ModelFailed",
        LogicalInput::ToolReturned { .. } => "ToolReturned",
        LogicalInput::SurfaceMutated { .. } => "SurfaceMutated",
        LogicalInput::SurfaceObserved { .. } => "SurfaceObserved",
        _ => "<other>",
    }
}

/// A compact, self-explaining summary of a recorded world log: the ordered
/// `kind` tags, with any `ModelFailed` event serialized inline so a failed live
/// call (e.g. an HTTP 404) is visible in a panic message.
pub fn world_log_summary(events: &[Event]) -> String {
    events
        .iter()
        .map(|e| {
            let name = variant_name(&e.input);
            if matches!(e.input, LogicalInput::ModelFailed { .. }) {
                let detail = serde_json::to_string(&e.input).unwrap_or_default();
                format!("{name}={detail}")
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A per-event `(edge, origin, variant)` trace, so a mode-projection assertion
/// failure prints the exact per-edge origin composition the fold saw.
pub fn edge_origin_trace(events: &[Event]) -> String {
    events
        .iter()
        .map(|e| format!("[e{} {:?} {}]", e.edge, e.origin, variant_name(&e.input)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a `cua-driver` executable is reachable on PATH — the GUI/AX gate's
/// availability probe (the AX half degrades to a SKIP when it is absent, mirroring
/// the live-LLM gate).
pub fn cua_driver_on_path() -> bool {
    std::process::Command::new("cua-driver")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A launched macOS surface client whose process is REAPED on drop. Holding the
/// child and killing it when the case ends matters for an automated runner: the
/// app is a child of the test process and inherits its stdout/stderr, so a leaked
/// app keeps the pipe open and a reader (`cargo test`'s capture, the e2e runner's
/// `Command::output()`, a `| tail`) blocks forever on EOF. Drop kills + waits so
/// the case never leaks a window or hangs the runner — on the normal path AND on a
/// panic unwind (the child dies during stack unwinding). When the pre-built app was
/// exec'd directly, the held child IS the app, so the kill terminates it; on the
/// `cargo xtask app run` fallback the child is the `cargo` launcher (best-effort).
pub struct LaunchedClient {
    child: Option<std::process::Child>,
}

impl Drop for LaunchedClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Launch the macOS surface client with `RUBBERDUX_SURFACE_APP_ID` /
/// `RUBBERDUX_SURFACE_PORT` set so it registers for `app_id` against our host, and
/// return a [`LaunchedClient`] guard that REAPS the process on drop (so the case
/// never leaks the app window or hangs the runner — see `LaunchedClient`).
///
/// Prefer exec'ing the PRE-BUILT debug app binary directly (it inherits this
/// process's environment, so the two `RUBBERDUX_SURFACE_*` vars actually reach the
/// app, and comes up within the settle window); fall back to `cargo xtask app run`
/// only when the pre-built binary is absent. Best-effort: a launch failure is
/// logged, not fatal — the backend invariants still run. Mirrors
/// `surface_drive.rs::launch_macos_surface_client` plus the reaping guard.
#[must_use = "drop the LaunchedClient at case end to reap the app and avoid a pipe hang"]
pub fn launch_macos_surface_client(app_id: &AppId, surface_port: u16) -> LaunchedClient {
    if let Some(binary) = prebuilt_debug_app_binary() {
        let mut command = std::process::Command::new(&binary);
        command
            .env("RUBBERDUX_SURFACE_APP_ID", app_id.as_str())
            .env("RUBBERDUX_SURFACE_PORT", surface_port.to_string());
        match command.spawn() {
            Ok(child) => {
                eprintln!(
                    "[surface] launched macOS surface client directly: {}",
                    binary.display()
                );
                return LaunchedClient { child: Some(child) };
            }
            Err(e) => eprintln!(
                "[surface] could not exec pre-built macOS app ({}): {e}; falling back to `cargo xtask app run`",
                binary.display()
            ),
        }
    } else {
        eprintln!(
            "[surface] no pre-built macOS app found (run `cargo xtask app build` for the fast \
             direct-exec AX path); falling back to `cargo xtask app run`"
        );
    }

    let mut command = std::process::Command::new("cargo");
    command
        .args(["xtask", "app", "run"])
        .env("RUBBERDUX_SURFACE_APP_ID", app_id.as_str())
        .env("RUBBERDUX_SURFACE_PORT", surface_port.to_string());
    if let Ok(dev_dir) = std::env::var("DEVELOPER_DIR") {
        command.env("DEVELOPER_DIR", dev_dir);
    }
    match command.spawn() {
        Ok(child) => {
            eprintln!("[surface] launched macOS surface client via `cargo xtask app run`");
            LaunchedClient { child: Some(child) }
        }
        Err(e) => {
            eprintln!("[surface] could not launch macOS surface client: {e}");
            LaunchedClient { child: None }
        }
    }
}

/// The path to the pre-built Debug `Rubberdux` app's inner executable, if present.
/// Produced by `cargo xtask app build`; exec'ing it directly is the env-correct,
/// fast launch the live AX half needs. `None` when the app has not been built.
pub fn prebuilt_debug_app_binary() -> Option<PathBuf> {
    let binary = PathBuf::from(
        "apps/macos/.build/DerivedData/Build/Products/Debug/\
         Rubberdux (Debug).app/Contents/MacOS/Rubberdux (Debug)",
    );
    binary.exists().then_some(binary)
}
