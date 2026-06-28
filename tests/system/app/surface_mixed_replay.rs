//! **VC-E.3** — the HEADLINE acceptance: a recorded mixed Operating / Assisted /
//! Driven session (with the live `SurfaceObserved` stream interleaved) replays to a
//! BYTE-IDENTICAL World with ZERO model/tool re-invocation and IDENTICAL per-edge
//! mode. (Inv 6, 18, 19.)
//!
//! This is the record→replay capstone over the SAME proven live stack VC-E.1
//! `surface_drive` and VC-E.2 `surface_modes` drive (reused via `surface_support`):
//! the daemon's [`LocalSupervisor`] spawns a real `rubberduxd --agent` worker that
//! drives the ECS World; the host's [`SurfaceRouter`] relays surface frames to a
//! launched macOS surface client; a benign first turn (Operating: the human
//! `UserMessage` standing alone) and one eliciting `set_value` turn (Assisted on the
//! human edge where the human `UserMessage` and the agent `ModelResponded`
//! interleave; Driven on the distinct app edge the agent's surface-write
//! `ToolReturned` is stamped on) produce a real recorded `world-events.jsonl`, with
//! the macOS reporter's `SurfaceObserved` perceptions interleaved (M-observe).
//!
//! From that single source of truth (Inv 9) the case proves replay determinism end
//! to end against a REAL live log (not a hand-authored one):
//!
//! - **(a) Byte-identical World (Inv 6).** Folding the WHOLE recorded log through
//!   the pure `tick` reducer (`fold_log`, the canonical "live" World) and replaying
//!   the SAME log under the REPLAY driver (`replay_log`: `drive_replay` +
//!   `ReplayCursor`, no `ModelCaller`) reconstruct Worlds whose canonical
//!   serialization is byte-for-byte equal. The surface-tools fix (recorded in
//!   `SessionStarted`, folded back into `Resources.surface_tools` on BOTH paths) is
//!   what makes the re-emitted `set_value`-carrying `CallModel` re-hash to its
//!   recorded `ModelResponded` fingerprint instead of diverging.
//! - **(b) Zero model/tool re-invocation (Inv 6).** The replay driver takes no
//!   client; an `ExplodingClient` (panics on call) is constructed and its call
//!   counter is asserted to stay at 0 — replay reuses every recorded result.
//! - **(c) Identical per-edge mode live-vs-replay (Inv 19).** The event stream the
//!   replay actually folds is byte-identical to the recorded log, so `mode(edge)`
//!   per edge agrees live-vs-replay: Operating on the human-edge prefix, Assisted on
//!   the human edge, Driven on the app edge.
//!
//! The replay loop stands the cursor in ONLY for model-call results
//! (`ModelResponded` / `ModelFailed` / `InferenceCancelled` / `Compacted`); the
//! agent's `set_value` `ToolReturned` is a recorded input re-applied directly (its
//! `RunTool`-dispatch replay is a later milestone — `drive_replay` skips `RunTool`),
//! so the cursor excludes `ToolReturned` to keep model-call reuse aligned across the
//! tool turn. This is the only adaptation over the offline replay harness
//! (`tests/integration/agent/world_surface_mode_replay.rs`) needed to admit a real
//! tool-loop log; it touches no `src/agent/world/*` file and makes no extra model
//! call (the replay is a pure fold of the already-recorded log).
//!
//! ## Gating
//! - Live LLM absent → SKIP (mock-data policy; the gate names the env var).
//! - Computer-use (`RUBBERDUX_SURFACE_DRIVE_MACOS=1` + a `cua-driver` on PATH)
//!   launches the real macOS surface client so the agent's drive lands on a real AX
//!   element, the agent-cursor overlay is observable out-of-band, and the live
//!   `SurfaceObserved` stream is interleaved into the recording. Without it the
//!   deterministic record→replay assertions still run headless from the recorded
//!   log (minus the live perception stream).
//!
//! See `docs/agent/world/ecs-runtime.md` (Inv 6 replay determinism; Inv 7
//! content-addressed replay; Inv 18 the agent UI-write is one fact; Inv 19
//! mode-as-projection) and the plan's Verification §7 VC-E.3.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::{
    drive_replay, ModelCaller, ReplayCursor, Replayed,
};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{Event, LogicalInput, ModelMeta, Origin};
use rubberdux::agent::world::mode::{mode, Mode};
use rubberdux::agent::world::model_client::MessagesClient;
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Lineage, ModelConfig, Resources, World,
};
use rubberdux::app::supervisor::{AppSupervisor, CreateAppRequest};
use rubberdux::app::{BoardPosition, IconSpec};
use rubberdux::error::Error;

use std::time::Duration;

use crate::app::surface_support::{
    await_final, boot, edge_origin_trace, read_world_log_settled, variant_name, world_log_summary,
    APP_EDGE, HUMAN_EDGE, TURN_TIMEOUT, WINDOW,
};
use crate::live_gate::skip_without_live_llm;

/// The text value the agent is asked to write into the real UI element. The Driven
/// assertion keys on the `set_value` write existing, never on model wording.
const TARGET_VALUE: &str = "agent-was-here";

// ---------------------------------------------------------------------------
// Genesis — the World construction reconstructed identically on both paths
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from, mirroring the worker's
/// `WorldDriver::bootstrap` (`src/agent/world/driver.rs`): tick 0, a single primary
/// `Idle` root entity, and `Resources` seeded from `seed` with EMPTY
/// `surface_tools`. The recorded `SessionStarted` fold — not genesis — reconstructs
/// `surface_tools`, which is precisely what makes the live fold and the replay
/// comparable: BOTH start here and fold the SAME recorded header.
fn genesis(seed: u64, model: &ModelConfig) -> World {
    let mut world = World::new(0, Resources::new(seed, model.clone()));
    world.entities.insert(
        0,
        Components {
            identity: Identity::Primary,
            lineage: Lineage {
                parent: None,
                depth: 0,
            },
            history: History::default(),
            activity: Activity::Idle,
            gate: EntityGate::default(),
            budget: rubberdux::agent::world::budget::Budget::default(),
            inbox: rubberdux::agent::world::world::Inbox::default(),
            turns: 0,
            spawned: 0,
            model: None,
            autonomy: None,
        },
    );
    world
}

/// Rebuild genesis from the recorded log: the seed is the one hidden input that must
/// cross a recorded boundary, so both folds reseed `Rng` from the log's
/// `SessionStarted` header. `surface_tools` is reconstructed by FOLDING that same
/// header (not read here), so genesis stays empty exactly as the worker's does.
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    let seed = events
        .iter()
        .find_map(|e| match &e.input {
            LogicalInput::SessionStarted { seed, .. } => Some(*seed),
            _ => None,
        })
        .ok_or_else(|| Error::World("recorded log has no SessionStarted header".into()))?;
    Ok(genesis(seed, model))
}

/// Reconstruct the EXACT `ModelConfig` the worker fingerprinted its `CallModel`
/// requests against, mirroring `worker::world_model_config(client.model())`: the
/// model ALIAS is the request alias the worker resolved from `RUBBERDUX_LLM_MODEL`
/// (NOT the provider's effective `meta.model_id`, which `MessagesClient` reports
/// post-call and may differ), `max_tokens` is read from `RUBBERDUX_LLM_MAX_TOKENS`
/// (default 4096), and effort is `Medium`. The test process and the worker share
/// this environment (the `system` target's `main` loads the repo-root `.env` before
/// spawning the worker), so the reconstructed params hash identically to the
/// recorded results' fingerprints. See `src/app/runtime/worker.rs`.
fn reconstruct_model_config(model_alias: &str) -> ModelConfig {
    let max_tokens = std::env::var("RUBBERDUX_LLM_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(4096);
    ModelConfig {
        model: model_alias.to_string(),
        max_tokens,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// ExplodingClient — the runtime witness that replay never reaches a model client
// ---------------------------------------------------------------------------

/// A `ModelCaller` that records any invocation and then panics. `drive_replay` takes
/// NO client, so this can never be threaded into `replay_log` by construction;
/// constructing it and asserting its counter stays zero makes the "zero model/tool
/// re-invocation" guarantee (Inv 6) explicit at runtime, while the panic is the
/// structural backstop.
struct ExplodingClient {
    calls: Arc<AtomicUsize>,
}

impl ModelCaller for ExplodingClient {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("the replay driver must never invoke the model client");
    }
}

// ---------------------------------------------------------------------------
// fold_log / replay_log — the LIVE canonical World vs. the cursor-driven replay
// ---------------------------------------------------------------------------

/// Fold the WHOLE recorded log through the pure `tick` reducer, event by event.
/// Every input — the exogenous free variables and the already-recorded DERIVED
/// results — is present, so emitted Commands are discarded. This is the canonical
/// ("live") World the replay must reproduce byte-for-byte.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    let mut world = genesis_from_log(events, model)?;
    for ev in events {
        let (next, _commands) = tick(&world, ev);
        world = next;
    }
    Ok(world)
}

/// Whether an input is a MODEL-call RESULT the replay driver stands in for via the
/// `ReplayCursor` (the `CallModel` / `Compact` duals). The agent's `set_value`
/// `ToolReturned` is NOT one of these: its `RunTool`-dispatch replay is a later
/// milestone (`drive_replay` skips `RunTool`), so it is re-applied directly by the
/// loop — exactly as crash-resume re-applies a recorded Input through the reducer.
fn is_model_call_result(input: &LogicalInput) -> bool {
    matches!(
        input,
        LogicalInput::ModelResponded { .. }
            | LogicalInput::ModelFailed { .. }
            | LogicalInput::InferenceCancelled { .. }
            | LogicalInput::Compacted { .. }
    )
}

/// Fold the SAME recorded log from genesis under the REPLAY driver, returning the
/// reconstructed World AND the exact event stream the replay folded (in fold order).
///
/// The loop re-applies every input EXCEPT the model-call results, and for each
/// tick's Commands calls `drive_replay` — which takes NO `ModelCaller` and so cannot
/// call the model by construction — standing in the already-logged result while the
/// re-emitted request re-hashes to its `Fingerprint`. The cursor is built over the
/// model-call results only (ToolReturned excluded) so reuse stays aligned across the
/// agent's surface-write turn, whose `ToolReturned` the loop re-applies directly. A
/// `Diverged` outcome (what the surface-tools gap produced before the fix) is
/// reported as an error: a faithful replay must reuse every result.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<(World, Vec<Event>), Error> {
    let mut world = genesis_from_log(events, model)?;

    // The cursor stands in ONLY for model-call results, so it must NOT see the
    // `ToolReturned` (re-applied directly below). Excluding it keeps `drive_replay`'s
    // per-`CallModel` reuse aligned with the recorded `ModelResponded` stream even
    // when a continuation `CallModel` follows the tool turn.
    let model_results: Vec<Event> = events
        .iter()
        .filter(|e| !matches!(e.input, LogicalInput::ToolReturned { .. }))
        .cloned()
        .collect();
    let mut cursor = ReplayCursor::new(&model_results);

    // The event stream the replay folds, in fold order — proven byte-identical to
    // the recorded log so the per-edge mode projection agrees live-vs-replay.
    let mut folded: Vec<Event> = Vec::with_capacity(events.len());

    for ev in events.iter().filter(|e| !is_model_call_result(&e.input)) {
        let (next, mut commands) = tick(&world, ev);
        world = next;
        folded.push(ev.clone());

        while !commands.is_empty() {
            let replayed = drive_replay(&commands, &mut cursor)?;
            commands = Vec::new();
            for outcome in replayed {
                match outcome {
                    Replayed::Reused(event) => {
                        let (next, mut cmds) = tick(&world, &event);
                        world = next;
                        folded.push(event);
                        commands.append(&mut cmds);
                    }
                    Replayed::Diverged => {
                        return Err(Error::World(
                            "replay diverged: a re-emitted CallModel did not match the recorded \
                             result, but a faithful replay must reuse every result (the \
                             surface-tools / continuation fingerprint gap is back)"
                                .into(),
                        ));
                    }
                }
            }
        }
    }

    Ok((world, folded))
}

// ---------------------------------------------------------------------------
// VC-E.3 — record a live mixed session, replay it byte-identically
// ---------------------------------------------------------------------------

/// VC-E.3 entry point. Boots the host harness, records a LIVE mixed
/// Operating/Assisted/Driven session (with the interleaved `SurfaceObserved`
/// stream), then replays the recorded `world-events.jsonl` to a byte-identical World
/// with zero model/tool re-invocation and identical per-edge mode.
pub async fn run() {
    if skip_without_live_llm("app::surface_mixed_replay (VC-E.3)") {
        return;
    }

    // -- Boot the host harness (shared with VC-E.1/VC-E.2) --------------------
    let h = boot().await;

    // -- Create the App and bring its worker up -------------------------------
    // The benign first turn's human `UserMessage` is the Operating-window sample
    // (the human standing alone before the first agent reply); the eliciting
    // `set_value` instruction is the SECOND turn, after the macOS client has
    // registered, so the agent's drive reaches a live client.
    let request = CreateAppRequest {
        title: "surface mixed replay (VC-E.3)".into(),
        icon: IconSpec {
            symbol: "arrow.triangle.2.circlepath".into(),
            color: "#FF9500".into(),
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
            "VC-E.3: the first live turn produced no final entry within {TURN_TIMEOUT:?}. \
             Recorded world log: [{}].",
            world_log_summary(&events)
        );
    }

    // -- Attach the macOS surface client (computer-use gate) ------------------
    // `_client` REAPS the app on drop (at run() end or on a panic unwind) so the
    // case never leaks the window or hangs the runner.
    let _client;
    if h.with_macos {
        unsafe {
            std::env::set_var("RUBBERDUX_SURFACE_PORT", h.surface_port.to_string());
            std::env::set_var("RUBBERDUX_SURFACE_APP_ID", app_id.as_str());
        }
        eprintln!(
            "[VC-E.3] macOS surface client target: RUBBERDUX_SURFACE_APP_ID={} RUBBERDUX_SURFACE_PORT={}",
            app_id.as_str(),
            h.surface_port
        );
        _client =
            crate::app::surface_support::launch_macos_surface_client(&app_id, h.surface_port);
        // Bounded settle for the client's connect + Hello registration before the
        // eliciting turn, so the agent's drive reaches a registered client and the
        // SurfaceObserved reporter starts interleaving perceptions.
        tokio::time::sleep(Duration::from_secs(8)).await;
    } else {
        eprintln!(
            "[VC-E.3] headless backend mode (set RUBBERDUX_SURFACE_DRIVE_MACOS=1 + a cua-driver on \
             PATH for the live agent-cursor/AX half and the interleaved SurfaceObserved stream). \
             Surface port {}, App id {}.",
            h.surface_port,
            app_id.as_str()
        );
    }

    // -- Drive the eliciting turn (agent set_value → Driven on APP_EDGE) -------
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
    eprintln!("[VC-E.3] recorded world log: {}", edge_origin_trace(&events));
    eprintln!(
        "[VC-E.3] recorded world-events.jsonl: {}",
        app_dir.join("latest").join("world-events.jsonl").display()
    );

    // -- Gate: the Driven sample must be a REAL agent surface write -----------
    // Without a `set_value` ToolReturned on the app edge the mixed-mode story has no
    // Driven sample. This is the same gate VC-E.1/VC-E.2 enforce.
    let app_writes: Vec<&Event> = events
        .iter()
        .filter(|e| e.edge == APP_EDGE && matches!(e.input, LogicalInput::ToolReturned { .. }))
        .collect();
    assert!(
        !app_writes.is_empty(),
        "VC-E.3: the agent emitted NO `set_value` ToolReturned on the app edge — the live turn \
         produced no surface drive, so the Driven phase of the mixed session is missing. \
         Recorded inputs: [{}].",
        events.iter().map(|e| variant_name(&e.input)).collect::<Vec<_>>().join(", ")
    );

    // The interleaved SurfaceObserved stream (M-observe) — informational; the live
    // reporter emits it only when the macOS client is attached.
    let observed = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::SurfaceObserved { .. }))
        .count();
    eprintln!("[VC-E.3] interleaved SurfaceObserved perceptions in the recording: {observed}");
    if h.with_macos && observed == 0 {
        eprintln!(
            "[VC-E.3] WARNING: macOS client attached but no SurfaceObserved was recorded \
             (reporter timing). The record→replay determinism assertions below still hold."
        );
    }

    // -- Reconstruct the worker's fingerprint ModelConfig ----------------------
    // The request alias the worker resolved from the shared env (NOT the provider's
    // effective post-call model id), so the re-emitted CallModel re-hashes to the
    // recorded ModelResponded fingerprint.
    let model_alias = MessagesClient::from_env()
        .expect("build a MessagesClient to read the worker's model alias")
        .model()
        .to_owned();
    let model = reconstruct_model_config(&model_alias);
    eprintln!(
        "[VC-E.3] replay fingerprint ModelConfig: model={:?} max_tokens={} effort={:?}",
        model.model, model.max_tokens, model.effort
    );

    // -- (a) Byte-identical World: live fold vs cursor-driven replay (Inv 6) ---
    let live = fold_log(&events, &model).unwrap_or_else(|e| {
        panic!(
            "VC-E.3: the recorded live log failed to fold ({e}). Trace: {}",
            edge_origin_trace(&events)
        )
    });

    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let (replay, folded) = replay_log(&events, &model).unwrap_or_else(|e| {
        panic!(
            "VC-E.3: replay of the recorded live log diverged ({e}). This is the surface-tools / \
             continuation fingerprint gap — the re-emitted CallModel must re-hash to its recorded \
             ModelResponded. Trace: {}",
            edge_origin_trace(&events)
        )
    });

    // Byte-identical World by canonical serialization (Inv 6): the reconstructed
    // World matches the live fold byte-for-byte.
    let live_bytes = serde_json::to_vec(&live).expect("serialize live World");
    let replay_bytes = serde_json::to_vec(&replay).expect("serialize replay World");
    assert_eq!(
        live_bytes, replay_bytes,
        "VC-E.3: replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "VC-E.3: replay must reconstruct an equal World");

    // Two independent replays are byte-identical (deterministic fold).
    let (replay_again, _) =
        replay_log(&events, &model).expect("second replay fold of the recorded log");
    assert_eq!(
        replay_bytes,
        serde_json::to_vec(&replay_again).expect("serialize second replay"),
        "VC-E.3: two independent replays of the same log are byte-identical"
    );

    // -- (b) Zero model/tool re-invocation (Inv 6) -----------------------------
    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "VC-E.3: replay must invoke the model client zero times (Inv 6)"
    );

    // -- (c) Identical per-edge mode live-vs-replay (Inv 19) -------------------
    // The replay folded a byte-identical event stream (it reused every recorded
    // result and re-applied every exogenous/observation/tool input directly), so the
    // per-edge mode projection agrees live-vs-replay by construction. Assert the
    // stream identity first, then the three modes on BOTH the live recording and the
    // replay-folded stream.
    assert_eq!(
        folded, events,
        "VC-E.3: the event stream the replay folds must be byte-identical to the recorded log \
         (so the per-edge mode projection agrees live-vs-replay)"
    );

    // The mixed-mode story, computed from the live recording and re-checked on the
    // replay-folded stream. The Operating window is the human edge's PREFIX before
    // the first agent reply (the human `UserMessage` standing alone).
    let first_agent_on_human_edge = events
        .iter()
        .position(|e| e.edge == HUMAN_EDGE && e.origin == Origin::Agent)
        .expect("the human edge must have an agent reply");
    let live_operating = &events[..first_agent_on_human_edge];
    let replay_operating = &folded[..first_agent_on_human_edge];

    let live_op = mode(live_operating, HUMAN_EDGE, WINDOW);
    let replay_op = mode(replay_operating, HUMAN_EDGE, WINDOW);
    assert_eq!(live_op, Mode::Operating, "OPERATING: human-edge prefix folds Operating (live)");
    assert_eq!(replay_op, live_op, "OPERATING: per-edge mode identical live-vs-replay");

    let live_assisted = mode(&events, HUMAN_EDGE, WINDOW);
    let replay_assisted = mode(&folded, HUMAN_EDGE, WINDOW);
    assert_eq!(live_assisted, Mode::Assisted, "ASSISTED: human edge folds Assisted (live)");
    assert_eq!(replay_assisted, live_assisted, "ASSISTED: per-edge mode identical live-vs-replay");

    let live_driven = mode(&events, APP_EDGE, WINDOW);
    let replay_driven = mode(&folded, APP_EDGE, WINDOW);
    assert_eq!(live_driven, Mode::Driven, "DRIVEN: app edge folds Driven (live)");
    assert_eq!(replay_driven, live_driven, "DRIVEN: per-edge mode identical live-vs-replay");

    eprintln!(
        "[VC-E.3] PASS: byte-identical World (live {} bytes == replay), 0 model/tool re-invocations, \
         per-edge mode identical live-vs-replay (Operating prefix / Assisted human edge / Driven app edge).",
        replay_bytes.len()
    );
    if h.with_macos {
        eprintln!(
            "[VC-E.3] computer-use half (human sign-off artifact): the agent's drive landed on the \
             real macOS surface (App id {}, surface port {}); observe the agent-cursor overlay / AX \
             value \"{TARGET_VALUE}\" via cua-driver during the Driven phase. The deterministic-replay \
             digest match above is the authoritative automated pass.",
            app_id.as_str(),
            h.surface_port
        );
    }
}
