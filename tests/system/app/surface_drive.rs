//! **VC-E.1** — the agent manipulates a real macOS UI element (Driven).
//!
//! Full stack: the daemon's [`LocalSupervisor`] spawns a real `rubberduxd --agent`
//! worker that drives the ECS World; the host's [`SurfaceRouter`] + surface-client
//! listener relay surface frames between that worker and the macOS surface client;
//! a single live `UserMessage` elicits the agent to call the `set_value` surface
//! tool, whose drive is applied to a real Accessibility element on screen.
//!
//! The acceptance has two halves, both of which require the agent to actually emit
//! a `set_value`:
//!
//! - **Backend (automated, deterministic):** the App's event log records the agent
//!   UI write EXACTLY ONCE as a `ToolReturned` (Inv 18) — with NO agent-origin
//!   `SurfaceMutated` (echo dedup) — and `mode(edge)` for the driven app edge folds
//!   `Driven` (Inv 19). These are asserted here from the worker's
//!   `world-events.jsonl` (the single source of truth, Inv 9).
//! - **Real element change (cua-driver AX checkpoint):** the addressed macOS
//!   element's value becomes the agent's `set_value`. This is observed out-of-band
//!   via cua-driver against the launched macOS surface client.
//!
//! Both halves are GATED on a real `set_value` from a live model call. The test
//! drives one turn, then asserts the backend invariants; only once the agent has
//! produced a surface drive does the GUI half (cua-driver AX) become meaningful.
//!
//! ## Gating
//! - Live LLM absent → SKIP (the standard live gate; mock-data policy).
//! - The GUI / AX half is opt-in via `RUBBERDUX_SURFACE_DRIVE_MACOS=1` (and a
//!   `cua-driver` on PATH); without it the backend invariants alone run headless,
//!   so the deterministic half stays runnable in CI.
//!
//! See `docs/agent/world/ecs-runtime.md` (Theme 2a/2b; Inv 18, 19) and the plan's
//! Verification §7 VC-E.1.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::broadcast;

use rubberdux::agent::runtime::port::EntryNotification;
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};
use rubberdux::agent::world::mode::{mode, Mode};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::supervisor::{AppSupervisor, CreateAppRequest};
use rubberdux::app::{AppId, BoardPosition, IconSpec};
use rubberdux::host::{run_surface_client_listener, SurfaceRouter};

use crate::live_gate::skip_without_live_llm;

/// The mode look-back window, matching the canonical backend test
/// (`tests/integration/agent/world_surface_mode_replay.rs`): larger than this
/// session's per-edge event count so the origin fold (not the window length) is
/// what is under test.
const WINDOW: usize = 16;

/// How long to wait for one turn to settle (a live model call plus, on the tool
/// turn, the continuation). Generous because a real provider round-trip is slow.
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// The text value the agent is asked to write into the real UI element. The
/// assertions key on this exact value (the element's value and the `set_value`
/// op's payload), never on model wording.
const TARGET_VALUE: &str = "agent-was-here";

/// VC-E.1 entry point. Boots the host harness, drives one live turn that should
/// elicit a `set_value`, and asserts the Driven-write invariants from the App's
/// event log. With the GUI gate set, it coordinates a macOS surface client so the
/// real element change can be observed via cua-driver.
pub async fn run() {
    if skip_without_live_llm("app::surface_drive (VC-E.1)") {
        return;
    }

    // -- Boot the host harness ------------------------------------------------
    // A real on-disk App store, the host's surface router, the surface-client
    // listener on an ephemeral port, and the subprocess-backed supervisor wired to
    // the SAME router (so a worker's `SurfaceDrive` routes to the registered macOS
    // client by App id). This is the in-process equivalent of `host::run`'s surface
    // wiring, minus VM/Telegram/gateway. See `src/host.rs` and `local_supervisor.rs`.
    // The App store home. Defaults to a leaked tempdir; overridable via
    // `RUBBERDUX_SURFACE_DRIVE_HOME` so an operator can inspect the worker's
    // `world-events.jsonl` artifact after a run.
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

    // Whether to coordinate the live macOS surface client + cua-driver AX half.
    // Opt-in: the deterministic backend invariants run headless by default so this
    // case stays runnable without a display/cua-driver. The App-id consistency
    // contract is the SAME in both modes (see below).
    let with_macos =
        std::env::var("RUBBERDUX_SURFACE_DRIVE_MACOS").is_ok() && cua_driver_on_path();

    // -- Create the App and bring its worker up -------------------------------
    // The App id is minted here (`AppId::now()` inside `create_app`); the macOS
    // client MUST register with this SAME id (`RUBBERDUX_SURFACE_APP_ID`) so the
    // SurfaceRouter relays the worker's drive to it. We therefore create first,
    // then point the client at `app.id` — the worker the agent drives and the App
    // id the client registers with are one and the same.
    let request = CreateAppRequest {
        title: "surface drive (VC-E.1)".into(),
        icon: IconSpec {
            symbol: "cursorarrow.rays".into(),
            color: "#34C759".into(),
        },
        position: BoardPosition { row: 0, column: 0 },
    };
    // A benign first turn so the worker connects and idles before the macOS client
    // is attached; the eliciting instruction is delivered as the SECOND turn, after
    // the client has registered, so the drive has a live client to reach.
    let app = supervisor
        .create_app(request, "You are attached to a macOS UI surface. Stand by.".into())
        .await
        .expect("create_app spawns the App's worker subprocess");
    let app_id = app.id.clone();
    let app_dir = store.app_dir(&app_id);

    // Confirm the worker connected and drove the first turn (a real model call).
    let mut entries = supervisor
        .subscribe_entries(&app_id)
        .await
        .expect("subscribe to the App's entry stream");
    if !await_final(&mut entries, TURN_TIMEOUT).await {
        // No assistant reply: dump the recorded world log so the failure is
        // self-explaining. A `model_failed` here means the World driver's live
        // model call failed — the Anthropic Messages dialect adapter posts to
        // the provider's `/v1/messages`; its `messages_url` helper folds the
        // base-URL `/v1` so a base already ending in `/v1` (the repo `.env` base
        // is `.../coding/v1`) is NOT doubled. See
        // src/provider/dialect/anthropic_messages.rs.
        let events = read_world_log_settled(&app_dir, Duration::from_secs(10));
        panic!(
            "VC-E.1: the first live turn produced no final entry within {TURN_TIMEOUT:?}. \
             Recorded world log: [{}]. A `model_failed` here is the live model call failing \
             (the Anthropic Messages dialect adapter posts to the provider's `/v1/messages`; \
             its `messages_url` helper folds the base-URL `/v1` so a base already ending in \
             `/v1` like the repo .env `.../coding/v1` is not doubled; see \
             src/provider/dialect/anthropic_messages.rs).",
            world_log_summary(&events)
        );
    }

    // -- Attach the macOS surface client (GUI gate) ---------------------------
    if with_macos {
        // Point the macOS app at OUR surface port and the SAME App id the worker
        // drives, then launch it and give it time to dial in + Hello-register with
        // the SurfaceRouter. The AX value-readback itself is performed by the
        // implementer via the cua-driver MCP (the acceptance "AX checkpoint").
        unsafe {
            std::env::set_var("RUBBERDUX_SURFACE_PORT", surface_port.to_string());
            std::env::set_var("RUBBERDUX_SURFACE_APP_ID", app_id.as_str());
        }
        eprintln!(
            "[VC-E.1] macOS surface client target: RUBBERDUX_SURFACE_APP_ID={} RUBBERDUX_SURFACE_PORT={}",
            app_id.as_str(),
            surface_port
        );
        launch_macos_surface_client(&app_id, surface_port);
        // Bounded settle for the client's connect + Hello registration before the
        // eliciting turn, so the agent's drive reaches a registered client.
        tokio::time::sleep(Duration::from_secs(8)).await;
    } else {
        eprintln!(
            "[VC-E.1] headless backend mode (set RUBBERDUX_SURFACE_DRIVE_MACOS=1 + a cua-driver on PATH for the live AX half). \
             Surface port {surface_port}, App id {}.",
            app_id.as_str()
        );
    }

    // -- Drive the eliciting turn ---------------------------------------------
    // One live model call should decide to call `set_value` on the surface. The
    // instruction names the tool and the exact value, but the model decides — the
    // assertions key on the recorded log shape and the element value, not wording.
    //
    // Element 0 names the surface ROOT — the one node that is ALWAYS realized in
    // the client's flattened AX subtree (`AccessibilitySurfaceDriveTarget`/
    // `SurfaceObservationReporter` both index `root` as 0). Surface perception is
    // not yet presented to the model (the `SurfaceObserved` carries only an opaque
    // `ax_digest`), so the model cannot discover deeper element ids on its own; it
    // can only honor a NAMED `(surface, element)`. Higher indices (e.g. 1) address
    // children that may not be realized — the client then logs the drive as
    // "element was not realized" and no on-screen value changes, even though the
    // backend invariants below still hold. Naming element 0 keeps the drive landing
    // on a realized element so the live AX checkpoint observes a real change.
    let mut entries = supervisor
        .subscribe_entries(&app_id)
        .await
        .expect("re-subscribe before the eliciting turn");
    supervisor
        .send_message(
            &app_id,
            format!(
                "Use the `set_value` tool to manipulate this macOS app's UI right now. \
                 Set surface 0, element 0 to the text \"{TARGET_VALUE}\" by calling \
                 `set_value` with `surface_ops` = [{{ \"op\": \"set_value\", \"surface\": 0, \
                 \"element\": 0, \"value\": \"{TARGET_VALUE}\" }}]. Call the tool; do not \
                 only describe it."
            ),
        )
        .await
        .expect("send the eliciting user message to the App's worker");
    assert!(
        await_final(&mut entries, TURN_TIMEOUT).await,
        "the eliciting turn must drive to a final entry"
    );

    // -- Assert the Driven-write invariants from the event log ----------------
    let events = read_world_log_settled(&app_dir, Duration::from_secs(10));
    assert!(
        !events.is_empty(),
        "the worker must have recorded a world event log at {}",
        app_dir.display()
    );

    let tool_returned: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::ToolReturned { .. }))
        .collect();

    // The gate: the agent must have actually emitted a `set_value`. If it did not,
    // dump the log so the failure is self-explaining (this is exactly the seam
    // where a missing tool-declaration shows up — see the BLOCKED note below).
    assert!(
        !tool_returned.is_empty(),
        "VC-E.1: the agent emitted NO `set_value` ToolReturned — the live turn produced \
         no surface drive. Recorded world-event inputs were: [{}]. \
         A real `set_value` requires the tool to be DECLARED in the CallModel request \
         (every CallModel currently carries `ToolSet::default()` — see \
         src/agent/world/systems/intake.rs).",
        events
            .iter()
            .map(|e| variant_name(&e.input))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Inv 18 — the agent UI write is recorded EXACTLY ONCE as a ToolReturned.
    assert_eq!(
        tool_returned.len(),
        1,
        "the agent UI write must be recorded exactly once as a ToolReturned (Inv 18)"
    );
    let agent_write = tool_returned[0];
    assert_eq!(
        agent_write.origin,
        Origin::Agent,
        "the surface-write ToolReturned is agent-origin"
    );

    // Inv 18 — there is NO agent-origin `SurfaceMutated` (the native echo of the
    // agent's own write is deduped by `Cause::Command`; the write's sole record is
    // the ToolReturned).
    assert_eq!(
        events
            .iter()
            .filter(|e| e.origin == Origin::Agent
                && matches!(e.input, LogicalInput::SurfaceMutated { .. }))
            .count(),
        0,
        "there must be NO agent-origin SurfaceMutated record (Inv 18)"
    );

    // Inv 19 — `mode(edge)` for the driven app edge folds `Driven`: the edge the
    // agent's surface write was stamped on shows an Agent actor and no Human.
    let driven_edge = agent_write.edge;
    assert_eq!(
        mode(&events, driven_edge, WINDOW),
        Mode::Driven,
        "mode(edge {driven_edge}) for the agent's surface write must fold Driven (Inv 19)"
    );

    eprintln!(
        "[VC-E.1] backend invariants hold: one ToolReturned, no agent SurfaceMutated, \
         mode(edge {driven_edge}) = Driven."
    );
    if with_macos {
        eprintln!(
            "[VC-E.1] AX checkpoint: verify via cua-driver that the macOS element's value \
             is now \"{TARGET_VALUE}\" (App id {}, surface port {surface_port}). \
             The authoritative M-apply evidence is the client's own `SurfaceDriveController` \
             log on stderr: with a realized element (0 = the surface root) it performs the \
             native `setAccessibilityValue` and emits NO \"element was not realized\" line.",
            app_id.as_str()
        );
    }
}

/// Await a turn's FINAL entry on the App's broadcast stream, returning `true` if
/// one arrives within `within`. A `Lagged` is skipped (the buffer advanced); a
/// closed channel returns `false`.
async fn await_final(
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
fn read_world_log_settled(app_dir: &Path, within: Duration) -> Vec<Event> {
    let deadline = Instant::now() + within;
    loop {
        let events = read_world_log(app_dir);
        if !events.is_empty() || Instant::now() >= deadline {
            return events;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Parse the App's world event log. Prefers the `latest` session symlink the
/// worker maintains; falls back to the most-recently-modified session log under
/// `sessions/`. Lines that do not parse as `Event` are skipped (the log may carry
/// a stratum-2 sibling shape on other lines in later milestones).
fn read_world_log(app_dir: &Path) -> Vec<Event> {
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

/// A compact, self-explaining summary of a recorded world log: the ordered
/// `kind` tags, with any `ModelFailed` event serialized inline so a failed live
/// call (e.g. an HTTP 404) is visible in the panic message.
fn world_log_summary(events: &[Event]) -> String {
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

/// The `LogicalInput` variant name, for the self-explaining diagnostic dump.
fn variant_name(input: &LogicalInput) -> &'static str {
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

/// Whether a `cua-driver` executable is reachable on PATH — the GUI/AX gate's
/// availability probe (the acceptance degrades to a SKIP of the AX half when it
/// is absent, mirroring the live-LLM gate).
fn cua_driver_on_path() -> bool {
    std::process::Command::new("cua-driver")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Launch the macOS surface client with `RUBBERDUX_SURFACE_APP_ID` /
/// `RUBBERDUX_SURFACE_PORT` set so it registers for `app_id` against our host. The
/// app reads these from its environment (`WhiteboardViewController`), connects its
/// `SurfaceSocket` to `127.0.0.1:<port>`, and Hello-registers.
///
/// Prefer exec'ing the PRE-BUILT debug app binary directly: it inherits this
/// process's environment (so the two `RUBBERDUX_SURFACE_*` vars actually reach the
/// app) and comes up within the eliciting-turn settle window. By contrast `open`
/// (and `cargo xtask app run`, which ends in `open`) launches via LaunchServices,
/// which does NOT forward arbitrary env vars and re-runs xcodegen + xcodebuild
/// first — too slow to register before the drive, and the app would read default
/// surface coordinates. We fall back to `cargo xtask app run` only when the
/// pre-built binary is absent (the operator is expected to `cargo xtask app build`
/// first for the live AX half). Best-effort: a launch failure is logged, not
/// fatal — the backend invariants still run.
fn launch_macos_surface_client(app_id: &AppId, surface_port: u16) {
    if let Some(binary) = prebuilt_debug_app_binary() {
        let mut command = std::process::Command::new(&binary);
        command
            .env("RUBBERDUX_SURFACE_APP_ID", app_id.as_str())
            .env("RUBBERDUX_SURFACE_PORT", surface_port.to_string());
        match command.spawn() {
            Ok(_child) => {
                eprintln!(
                    "[VC-E.1] launched macOS surface client directly: {}",
                    binary.display()
                );
                return;
            }
            Err(e) => eprintln!(
                "[VC-E.1] could not exec pre-built macOS app ({}): {e}; falling back to `cargo xtask app run`",
                binary.display()
            ),
        }
    } else {
        eprintln!(
            "[VC-E.1] no pre-built macOS app found (run `cargo xtask app build` for the fast \
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
        Ok(_child) => eprintln!("[VC-E.1] launched macOS surface client via `cargo xtask app run`"),
        Err(e) => eprintln!("[VC-E.1] could not launch macOS surface client: {e}"),
    }
}

/// The path to the pre-built Debug `Rubberdux` app's inner executable, if present.
/// Produced by `cargo xtask app build`; exec'ing it directly is the env-correct,
/// fast launch the live AX half needs (see `launch_macos_surface_client`). `None`
/// when the app has not been built, in which case the caller falls back to
/// `cargo xtask app run`.
fn prebuilt_debug_app_binary() -> Option<PathBuf> {
    let binary = PathBuf::from(
        "apps/macos/.build/DerivedData/Build/Products/Debug/\
         Rubberdux (Debug).app/Contents/MacOS/Rubberdux (Debug)",
    );
    binary.exists().then_some(binary)
}
