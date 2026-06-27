//! **VC-E.2** — the three fluid modes project from input ORIGIN per edge (Inv 19).
//!
//! Full stack (the SAME proven boot path as VC-E.1 `surface_drive`, reused via
//! `surface_support`): the daemon's [`LocalSupervisor`] spawns a real
//! `rubberduxd --agent` worker that drives the ECS World; the host's
//! [`SurfaceRouter`] relays surface frames to a launched macOS surface client; a
//! benign first turn and one eliciting `set_value` turn produce a real recorded
//! `world-events.jsonl`. From that single source of truth (Inv 9) this case asserts
//! that the pure mode projection `mode(edge)` (`src/agent/world/mode.rs`) folds
//! each per-edge origin window to the mode its story demands:
//!
//! - **Operating** — a window of only HUMAN-origin events on the human edge folds
//!   `Operating` (only the human acting). Realized as the human edge's prefix
//!   BEFORE the first agent reply (the human `UserMessage` standing alone).
//! - **Assisted** — the human edge's full window, where the human `UserMessage`
//!   and the agent `ModelResponded` are interleaved on the SAME edge, folds
//!   `Assisted` (the agent co-piloting alongside the human).
//! - **Driven** — the DISTINCT app edge the agent's `set_value` `ToolReturned` is
//!   stamped on folds `Driven` (the agent steering; no human on that edge).
//! - **Empty window → Operating** and **a System/neutral-only window → Operating,
//!   NOT Driven** — `Driven` requires a real Agent/Peer actor sample and NEVER
//!   arises from mere Human-absence over neutral events.
//!
//! These hold concurrently in one session: `mode(HUMAN_EDGE)` = Assisted AND
//! `mode(APP_EDGE)` = Driven, simultaneously (Inv 19 concurrent per-edge modes).
//!
//! ## Gating
//! - Live LLM absent → SKIP (mock-data policy; the gate names the env var).
//! - Computer-use (`RUBBERDUX_SURFACE_DRIVE_MACOS=1` + a `cua-driver` on PATH)
//!   launches the real macOS surface client so the agent's drive lands on a real
//!   AX element and the agent-cursor overlay / AX checkpoint can be observed
//!   out-of-band; without it the deterministic per-edge mode assertions still run
//!   headless from the recorded log.
//!
//! See `docs/agent/world/ecs-runtime.md` (Mode-as-projection; Inv 19) and the
//! plan's Verification §7 VC-E.2.

use std::time::Duration;

use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};
use rubberdux::agent::world::mode::{mode, Mode};
use rubberdux::app::supervisor::{AppSupervisor, CreateAppRequest};
use rubberdux::app::{BoardPosition, IconSpec};

use crate::app::surface_support::{
    await_final, boot, edge_origin_trace, read_world_log_settled, variant_name, APP_EDGE,
    HUMAN_EDGE, TURN_TIMEOUT, WINDOW,
};
use crate::live_gate::skip_without_live_llm;

/// The text value the agent is asked to write into the real UI element. The
/// Driven assertion keys on the `set_value` write existing, never on model wording.
const TARGET_VALUE: &str = "agent-was-here";

/// VC-E.2 entry point. Boots the host harness, drives a benign first turn and one
/// eliciting `set_value` turn, then asserts the three per-edge modes plus the
/// empty- and neutral-window results from the App's recorded `world-events.jsonl`.
pub async fn run() {
    if skip_without_live_llm("app::surface_modes (VC-E.2)") {
        return;
    }

    // -- Boot the host harness (shared with VC-E.1) ---------------------------
    let h = boot().await;

    // -- Create the App and bring its worker up -------------------------------
    // A benign first turn so the worker connects and idles before the macOS client
    // is attached; the eliciting `set_value` instruction is the SECOND turn, after
    // the client has registered, so the drive has a live client to reach. The
    // first turn's human `UserMessage` is the Operating-window human sample.
    let request = CreateAppRequest {
        title: "surface modes (VC-E.2)".into(),
        icon: IconSpec {
            symbol: "cursorarrow.motionlines".into(),
            color: "#5856D6".into(),
        },
        position: BoardPosition { row: 0, column: 0 },
    };
    let app = h
        .supervisor
        .create_app(
            request,
            "You are attached to a macOS UI surface. Stand by.".into(),
        )
        .await
        .expect("create_app spawns the App's worker subprocess");
    let app_id = app.id.clone();
    let app_dir = h.store.app_dir(&app_id);

    let mut entries = h
        .supervisor
        .subscribe_entries(&app_id)
        .await
        .expect("subscribe to the App's entry stream");
    if !await_final(&mut entries, TURN_TIMEOUT).await {
        let events = read_world_log_settled(&app_dir, Duration::from_secs(10));
        panic!(
            "VC-E.2: the first live turn produced no final entry within {TURN_TIMEOUT:?}. \
             Recorded world log: [{}].",
            edge_origin_trace(&events)
        );
    }

    // -- Attach the macOS surface client (computer-use gate) ------------------
    // `_client` REAPS the app on drop (at run() end or on a panic unwind) so the
    // case never leaks the window or hangs the runner — the app inherits this
    // process's stdout/stderr, and a survivor would hold that pipe open forever.
    let _client;
    if h.with_macos {
        unsafe {
            std::env::set_var("RUBBERDUX_SURFACE_PORT", h.surface_port.to_string());
            std::env::set_var("RUBBERDUX_SURFACE_APP_ID", app_id.as_str());
        }
        eprintln!(
            "[VC-E.2] macOS surface client target: RUBBERDUX_SURFACE_APP_ID={} RUBBERDUX_SURFACE_PORT={}",
            app_id.as_str(),
            h.surface_port
        );
        _client =
            crate::app::surface_support::launch_macos_surface_client(&app_id, h.surface_port);
        // Bounded settle for the client's connect + Hello registration before the
        // eliciting turn, so the agent's drive reaches a registered client.
        tokio::time::sleep(Duration::from_secs(8)).await;
    } else {
        eprintln!(
            "[VC-E.2] headless backend mode (set RUBBERDUX_SURFACE_DRIVE_MACOS=1 + a cua-driver on \
             PATH for the live agent-cursor/AX half). Surface port {}, App id {}.",
            h.surface_port,
            app_id.as_str()
        );
    }

    // -- Drive the eliciting turn (agent set_value → Driven on APP_EDGE) -------
    // One live model call should decide to call `set_value` on surface 0, element 0
    // (the surface ROOT — the one node always realized in the client's flattened AX
    // subtree, per `surface_drive.rs`). This turn's human `UserMessage` + the agent
    // `ModelResponded` interleave on the human edge (Assisted), while the resulting
    // `ToolReturned` lands on the distinct app edge (Driven) — concurrent per-edge
    // modes in one turn.
    let mut entries = h
        .supervisor
        .subscribe_entries(&app_id)
        .await
        .expect("re-subscribe before the eliciting turn");
    h.supervisor
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

    // -- Read the recorded log (the single source of truth, Inv 9) ------------
    let events = read_world_log_settled(&app_dir, Duration::from_secs(10));
    assert!(
        !events.is_empty(),
        "the worker must have recorded a world event log at {}",
        app_dir.display()
    );
    eprintln!("[VC-E.2] recorded world log: {}", edge_origin_trace(&events));

    assert_modes(&events);

    eprintln!(
        "[VC-E.2] PASS: mode(HUMAN_EDGE prefix)=Operating, mode(HUMAN_EDGE)=Assisted, \
         mode(APP_EDGE)=Driven, empty window=Operating, neutral-only window=Operating (not Driven)."
    );
    if h.with_macos {
        eprintln!(
            "[VC-E.2] computer-use half: the agent's drive landed on the real macOS surface \
             (App id {}, surface port {}); observe the agent-cursor overlay / AX value via \
             cua-driver. Backend mode projection is the authoritative pass above.",
            app_id.as_str(),
            h.surface_port
        );
    }
}

/// Assert the full VC-E.2 mode-projection story from the recorded per-edge origins.
/// Every assertion reads the SHAPE of the origin window the fold sees (composition),
/// never model wording, so it is robust across model variation.
fn assert_modes(events: &[Event]) {
    // === DRIVEN — the agent's surface write on the distinct app edge ===========
    // The gate: the agent must have actually emitted a `set_value`. Without it the
    // app edge has no agent sample and the Driven story cannot be told.
    let app_writes: Vec<&Event> = events
        .iter()
        .filter(|e| e.edge == APP_EDGE && matches!(e.input, LogicalInput::ToolReturned { .. }))
        .collect();
    assert!(
        !app_writes.is_empty(),
        "VC-E.2: the agent emitted NO `set_value` ToolReturned on the app edge — the live turn \
         produced no surface drive, so the Driven story cannot be driven. Recorded inputs: [{}]. \
         A real `set_value` requires the tool to be DECLARED in the CallModel request (see \
         src/agent/world/systems/intake.rs / src/agent/world/driver.rs surface_tool_names).",
        events.iter().map(|e| variant_name(&e.input)).collect::<Vec<_>>().join(", ")
    );
    // The agent write is agent-origin (the actor sample the Driven fold needs).
    assert!(
        app_writes.iter().all(|e| e.origin == Origin::Agent),
        "every app-edge surface write must be agent-origin (the Driven actor sample)"
    );
    // No human ever acts on the app edge (else it would be Assisted, not Driven).
    assert_eq!(
        events
            .iter()
            .filter(|e| e.edge == APP_EDGE && e.origin == Origin::Human)
            .count(),
        0,
        "no human-origin event may appear on the app edge (it is the agent's Driven edge)"
    );
    assert_eq!(
        mode(events, APP_EDGE, WINDOW),
        Mode::Driven,
        "DRIVEN: mode(APP_EDGE) must fold Driven (agent-only actor window). Trace: {}",
        edge_origin_trace(events)
    );

    // === ASSISTED — human + agent interleaved on the SAME (human) edge =========
    // The human edge carries both the human `UserMessage`(s) and the agent
    // `ModelResponded`(s); interleaved actors fold Assisted (Inv 19, Theme 3b).
    let human_on_human_edge = events
        .iter()
        .any(|e| e.edge == HUMAN_EDGE && e.origin == Origin::Human);
    let agent_on_human_edge = events
        .iter()
        .any(|e| e.edge == HUMAN_EDGE && e.origin == Origin::Agent);
    assert!(
        human_on_human_edge && agent_on_human_edge,
        "ASSISTED requires BOTH a human and an agent sample on the human edge; got human={human_on_human_edge} agent={agent_on_human_edge}. Trace: {}",
        edge_origin_trace(events)
    );
    assert_eq!(
        mode(events, HUMAN_EDGE, WINDOW),
        Mode::Assisted,
        "ASSISTED: mode(HUMAN_EDGE) must fold Assisted (interleaved human + agent). Trace: {}",
        edge_origin_trace(events)
    );

    // === OPERATING — a window of only the human acting on the human edge ========
    // The human edge's PREFIX before the first agent reply is a window where only
    // the human has acted (the human `UserMessage` standing alone). It folds
    // Operating — the human in direct control, the agent standing back (Theme 3a).
    let first_agent_on_human_edge = events
        .iter()
        .position(|e| e.edge == HUMAN_EDGE && e.origin == Origin::Agent)
        .expect("the human edge must have an agent reply (asserted above)");
    let operating_window = &events[..first_agent_on_human_edge];
    assert!(
        operating_window
            .iter()
            .any(|e| e.edge == HUMAN_EDGE && e.origin == Origin::Human),
        "OPERATING window must contain a real human sample on the human edge before the first \
         agent reply. Trace: {}",
        edge_origin_trace(operating_window)
    );
    assert!(
        !operating_window
            .iter()
            .any(|e| e.edge == HUMAN_EDGE
                && matches!(e.origin, Origin::Agent | Origin::Peer)),
        "OPERATING window must contain NO agent/peer sample on the human edge. Trace: {}",
        edge_origin_trace(operating_window)
    );
    assert_eq!(
        mode(operating_window, HUMAN_EDGE, WINDOW),
        Mode::Operating,
        "OPERATING: a human-only window on the human edge must fold Operating. Trace: {}",
        edge_origin_trace(operating_window)
    );

    // === EMPTY WINDOW → Operating ==============================================
    // The explicit empty-window result (Theme 3a): no actor events at all.
    assert_eq!(
        mode(&[], HUMAN_EDGE, WINDOW),
        Mode::Operating,
        "EMPTY: an empty event window must fold Operating on the human edge"
    );
    assert_eq!(
        mode(&[], APP_EDGE, WINDOW),
        Mode::Operating,
        "EMPTY: an empty event window must fold Operating on the app edge"
    );

    // === NEUTRAL-ONLY WINDOW → Operating, NOT Driven ===========================
    // A window of ONLY System/neutral events (e.g. SessionStarted, and the live
    // SurfaceObserved perceptions the macOS reporter emits) has no actor sample at
    // all. It folds Operating — PROVING Driven NEVER arises from mere Human-absence
    // over neutral events (Inv 19, Theme 3b). The neutral window is real recorded
    // data filtered to System-origin, so this is not a synthetic claim.
    let neutral_only: Vec<Event> = events
        .iter()
        .filter(|e| e.origin == Origin::System && e.edge == HUMAN_EDGE)
        .cloned()
        .collect();
    assert!(
        !neutral_only.is_empty(),
        "expected at least one System/neutral event (SessionStarted) on the human edge"
    );
    let neutral_mode = mode(&neutral_only, HUMAN_EDGE, WINDOW);
    assert_eq!(
        neutral_mode,
        Mode::Operating,
        "NEUTRAL: a System/neutral-only window must fold Operating. Trace: {}",
        edge_origin_trace(&neutral_only)
    );
    assert_ne!(
        neutral_mode,
        Mode::Driven,
        "NEUTRAL: a System/neutral-only window must NEVER fold Driven (Driven needs a real \
         Agent/Peer actor sample, not mere Human-absence). Trace: {}",
        edge_origin_trace(&neutral_only)
    );
}
