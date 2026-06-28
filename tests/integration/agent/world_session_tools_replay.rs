//! A hand-authored `set_value` session — whose root `CallModel` carried a
//! NON-EMPTY `ToolSet(["set_value"])` — folds and replays BYTE-IDENTICALLY with
//! zero model calls, proving the surface-tools replay-determinism gap is closed.
//!
//! Before the fix, `surface_tools` was a live-only seed: `WorldDriver::bootstrap`
//! wrote `Resources.surface_tools = ["set_value"]` directly, but the value never
//! crossed a recorded boundary. So a replay genesis rebuilt it EMPTY, the
//! re-emitted root `CallModel` carried an empty `ToolSet`, its `Fingerprint` no
//! longer matched the recorded `ModelResponded`, and `drive_replay` returned
//! `Diverged` (a hard replay error) — while the reconstructed `Resources` also
//! differed in bytes. Every existing replay sink used a TOOL-LESS log, so the gap
//! was invisible.
//!
//! The fix records `surface_tools` in the `SessionStarted` header and folds it
//! back into `Resources.surface_tools` (SurfaceSystem) — the SINGLE source of
//! truth, reconstructed IDENTICALLY on the live and replay paths. This test is the
//! offline witness: it authors a log whose recorded `ModelResponded` fingerprint
//! is computed against `ToolSet(["set_value"])`, then asserts the replay REUSES it
//! (never `Diverged`) and reconstructs a byte-identical World — including
//! `Resources.surface_tools` — with the model client invoked zero times.
//!
//! It touches no `src/agent/world/*` file and makes no live model call, so it
//! needs no credentials and runs on every developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), Inv 7
//! (content-addressed replay — the fingerprint), SurfaceSystem (surface tools).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::{ModelCaller, ToolSet, fingerprint_call};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8). Inert here — no
// System draws — but it must reseed identically for the two folds to match.
const SEED: u64 = 7;

// The whiteboard App's sole surface tool, the value `WorldDriver::bootstrap`
// records into `SessionStarted` and the fold seeds into `Resources.surface_tools`.
const SET_VALUE: &str = "set_value";

// ---------------------------------------------------------------------------
// Genesis — empty surface tools; the SessionStarted fold reconstructs them
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary `Idle` root
/// entity, and `Resources` seeded from `seed` with EMPTY `surface_tools`. The
/// recorded `SessionStarted` fold — not genesis — reconstructs `surface_tools`,
/// which is precisely what makes the live and replay Worlds comparable: BOTH start
/// here and fold the SAME recorded header. Mirrors the other replay harnesses'
/// genesis (no per-harness special-casing of surface tools).
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
        },
    );
    world
}

/// Rebuild genesis from the recorded log: the seed is the one hidden input that
/// must cross a recorded boundary, so replay reseeds `Rng` from the log's
/// `SessionStarted` header. `surface_tools` is reconstructed by FOLDING that same
/// header (not read here), so genesis stays empty exactly as the live path's does.
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

/// The world-default `ModelConfig`. Its `model` id rides in the request the
/// re-emitted `CallModel` fingerprints, so the recorded `ModelResponded` below is
/// stamped with a fingerprint computed against THIS exact value; it makes no call.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-session-tools-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// ExplodingClient — the runtime witness that replay never reaches a model client
// ---------------------------------------------------------------------------

/// A `ModelCaller` that records any invocation and then panics. `drive_replay`
/// takes NO client, so this can never be threaded into `replay_log` by
/// construction; constructing it and asserting its counter stays zero makes the
/// "zero model calls" guarantee (Inv 6) explicit, while the panic is the backstop.
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
/// `ModelResponded` — is present, so emitted Commands are discarded. This is the
/// canonical ("live") World the replay must reproduce byte-for-byte.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    Ok(replay::fold_log(genesis_from_log(events, model)?, events))
}

/// Fold the SAME recorded log from genesis under the promoted REPLAY driver
/// (`replay::replay_world`): re-apply the exogenous events (`replay::is_exogenous`)
/// directly while the `ReplayCursor` stands in for every DERIVED result, gated on the
/// re-emitted request re-hashing to its `Fingerprint`. A `Diverged` outcome (what the
/// surface-tools gap produced before the fix — an empty `ToolSet` on replay) is reported
/// as an error: a faithful replay must reuse every result.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(genesis_from_log(events, model)?, events, |input| {
        !replay::is_exogenous(input)
    })
}

// ---------------------------------------------------------------------------
// The hand-authored set_value session (CallModel carried ToolSet["set_value"])
// ---------------------------------------------------------------------------

/// Author the log. The recorded `SessionStarted` carries `surface_tools =
/// ["set_value"]`; the recorded `ModelResponded` carries the fingerprint the
/// re-emitted root `CallModel` re-hashes to — computed against the one-message
/// user History, the NON-EMPTY `ToolSet(["set_value"])` IntakeSystem reads from the
/// folded `surface_tools`, and the world-default params. A replay that fails to
/// reconstruct `surface_tools` would re-hash against an EMPTY ToolSet and diverge.
fn authored_log(model: &ModelConfig) -> Vec<Event> {
    let user_text = "tidy the form";
    let assistant_text = "done";

    // Intake re-emits exactly this request for a fresh root entity: its History is
    // the single user message, and — because the root carries surface tools — its
    // ToolSet is the non-empty `["set_value"]`.
    let request_history = History(vec![Msg {
        role: Role::User,
        content: vec![Block::Text {
            text: user_text.into(),
        }],
    }]);
    let fingerprint = fingerprint_call(
        &request_history,
        &ToolSet(vec![SET_VALUE.into()]),
        model,
    )
    .expect("compute the recorded request fingerprint over the non-empty ToolSet");

    vec![
        // tick 0 — session header carrying the App's surface tools (the fix).
        Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input: LogicalInput::SessionStarted {
                seed: SEED,
                surface_tools: vec![SET_VALUE.into()],
            },
        },
        // tick 1 — the human asks the surface-capable root to act.
        Event {
            origin: Origin::Human,
            edge: 0,
            at: 1,
            wall: None,
            input: LogicalInput::UserMessage {
                to: 0,
                text: user_text.into(),
            },
        },
        // tick 2 — the recorded answer, fingerprinted over the non-empty ToolSet.
        Event {
            origin: Origin::Agent,
            edge: 0,
            at: 2,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd: 0,
                entity: 0,
                fingerprint,
                blocks: vec![Block::Text {
                    text: assistant_text.into(),
                }],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: model.model.clone(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// The test: a non-empty-ToolSet session replays byte-identically (Inv 6/7)
// ---------------------------------------------------------------------------

/// A `set_value` session whose root `CallModel` carried `ToolSet(["set_value"])`
/// folds and replays to a BYTE-IDENTICAL World with zero model calls. This FAILED
/// before the surface-tools fix (the replay re-emitted an empty ToolSet, diverged,
/// and reconstructed `Resources.surface_tools` empty); the recorded-and-folded
/// `surface_tools` closes both failures.
#[test]
fn set_value_session_replays_byte_identical_with_reconstructed_surface_tools() {
    let model = offline_model();
    let events = authored_log(&model);

    // The canonical ("live") World: re-apply every recorded Event through `tick`.
    let live = fold_log(&events, &model).expect("the set_value log folds");

    // The fold reconstructed the App's surface tools from the recorded header —
    // the single source of truth (before the fix this stayed empty).
    assert_eq!(
        live.resources.surface_tools,
        ToolSet(vec![SET_VALUE.into()]),
        "the SessionStarted fold reconstructs Resources.surface_tools on the live path"
    );

    // The turn settled to Idle with user + assistant text, proving the recorded
    // result was actually consumed (not skipped).
    let entity = live.entities.get(&0).expect("primary entity present");
    assert!(matches!(entity.activity, Activity::Idle), "the turn settled to Idle");
    assert_eq!(entity.history.0.len(), 2, "History holds user + assistant");

    // --- Replay determinism (Inv 6/7) -----------------------------------------

    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    // The load-bearing assertion: BEFORE the fix this `.expect` panics because the
    // re-emitted CallModel carried an empty ToolSet, mismatched the recorded
    // fingerprint, and `replay_log` returned the `Diverged` error.
    let replay =
        replay_log(&events, &model).expect("replay REUSES the recorded result (never Diverged)");

    // Replay reconstructed `surface_tools` identically via the SAME folded header.
    assert_eq!(
        replay.resources.surface_tools,
        ToolSet(vec![SET_VALUE.into()]),
        "replay reconstructs Resources.surface_tools identically (single source of truth)"
    );

    // Byte-identical World by canonical serialization (Inv 6): the reconstructed
    // World — including `Resources.surface_tools` — matches the live fold.
    let live_bytes = serde_json::to_vec(&live).expect("serialize live World");
    let replay_bytes = serde_json::to_vec(&replay).expect("serialize replay World");
    assert_eq!(
        live_bytes, replay_bytes,
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "replay must reconstruct an equal World");

    // Two independent replays are byte-identical (deterministic fold).
    let replay_again = replay_log(&events, &model).expect("second replay fold");
    assert_eq!(
        replay_bytes,
        serde_json::to_vec(&replay_again).expect("serialize second replay"),
        "two independent replays of the same log are byte-identical"
    );

    // ZERO model/tool re-invocation: the replay driver invoked no client.
    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "replay must invoke the model client zero times (Inv 6)"
    );
}
