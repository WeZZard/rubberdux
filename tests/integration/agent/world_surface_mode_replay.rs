//! A hand-authored mixed-mode session that folds a synthetic log of Operating /
//! Assisted / Driven Events (with an interleaved `SurfaceObserved` stream) and
//! proves the UI differentiator's backend determinism — with NO macOS.
//!
//! This is the U-workstream replay GATE (VC-U.5): it composes the same
//! `src/agent/world/` stack the walking skeleton wires (genesis, the pure `tick`
//! reducer, and the REPLAY driver `drive_replay` + `ReplayCursor`), but over a
//! log that exercises the surface view and the per-edge mode projection. It
//! touches no `src/agent/world/*` file. It asserts three things at once:
//!
//! - **Surface view** — the folded `Resources.surfaces` matches the expected
//!   per-surface `version`/`ax_digest` (human `SurfaceMutated`, a `SurfaceObserved`
//!   perception refresh, and the agent `set_value` `ToolReturned` projection all
//!   land where they should). (SurfaceSystem.)
//! - **Mode per edge (Inv 19)** — `mode(log, edge, window)` folds to `Operating`
//!   on a human-only edge, `Assisted` where Human and Agent interleave, and
//!   `Driven` on an Agent-only edge, simultaneously and per-edge. The interleaved
//!   `SurfaceObserved` stream is `System`-origin and therefore mode-neutral.
//! - **The agent write is one fact (Inv 18)** — the agent UI write is recorded
//!   EXACTLY ONCE as a `ToolReturned`; there is NO agent-origin `SurfaceMutated`
//!   in the log, and the surface view is bumped exactly once per write (a
//!   projection of that single record, never re-authored).
//!
//! Then it proves **replay determinism (Inv 6)**: replaying the log under the
//! replay driver — with a `ModelCaller` that PANICS if invoked — reconstructs a
//! World byte-identical to the live fold, with ZERO model/tool re-invocation.
//!
//! It makes NO live model call (a pure fold + replay of a hand-authored log), so
//! it needs no credentials and runs on every developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), Inv 18 (the
//! agent UI-write echo is one fact), Inv 19 (mode-as-projection).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::ModelCaller;
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::inputs::{
    Capabilities, Event, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
};
use rubberdux::agent::world::mode::{Mode, mode};
use rubberdux::agent::world::surface::{
    Hash, Selection, SurfaceOp, Viewport, WindowState, surface_ops_from_tool_result,
};
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

// The edges this session runs over, named after the relationship each models so
// the per-edge mode assertions read as phrases.
const EDGE_OPERATING: u32 = 0;
const EDGE_ASSISTED: u32 = 1;
const EDGE_DRIVEN: u32 = 2;

// The mode look-back window. Larger than this session's per-edge event count, so
// every authored Event on an edge is in view (the window itself is not the
// subject under test here — the origin fold is).
const WINDOW: usize = 16;

// The hidden RNG seed crossing the recorded boundary (Inv 8). Inert here — no
// System draws — but it must reseed identically for the two folds to match.
const SEED: u64 = 7;

// The surfaces this session touches. S0 is human-only territory, S1 is shared,
// S2 is where the agent writes.
const SURFACE_HUMAN: u32 = 0;
const SURFACE_SHARED: u32 = 1;
const SURFACE_AGENT: u32 = 2;

// ---------------------------------------------------------------------------
// Genesis — the World construction no System performs from `SessionStarted`
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary entity at
/// `Idle`, and `Resources` seeded from the session's `seed`. The shell owns
/// genesis (no P0 System creates the entity from `SessionStarted`), and BOTH the
/// live fold and the replay reconstruct it the SAME way — which is what makes the
/// two Worlds comparable byte-for-byte. Mirrors the walking-skeleton genesis the
/// replay harness depends on.
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
            model: None,
        },
    );
    world
}

/// Rebuild genesis from the recorded log: the seed is the one hidden input that
/// must cross a recorded boundary, so replay reseeds `Rng` from the log's
/// `SessionStarted` header rather than from any live source.
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

/// The world-default `ModelConfig`. Its `model` id rides in the request the
/// re-emitted `CallModel` fingerprints, so the recorded `ModelResponded` below is
/// stamped with a fingerprint computed against THIS exact value; it makes no real
/// call.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-surface-mode-replay".into(),
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
/// "zero model/tool calls" guarantee (Inv 6) explicit at runtime, while the panic
/// is the structural backstop.
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
// fold_log — the LIVE canonical World: re-apply every recorded Event via `tick`
// ---------------------------------------------------------------------------

/// Fold the WHOLE recorded log through the pure `tick` reducer, event by event.
/// Every input — the exogenous free variables and the already-recorded DERIVED
/// results — is present in the log, so there is nothing to dispatch and the
/// emitted Commands are discarded. This is the canonical ("live") World the
/// replay must reproduce byte-for-byte.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    Ok(replay::fold_log(genesis_from_log(events, model)?, events))
}

// ---------------------------------------------------------------------------
// replay_log — the REPLAY driver: stand in model-call results from the cursor
// ---------------------------------------------------------------------------

/// Fold the SAME recorded log from genesis under the promoted REPLAY driver
/// (`replay::replay_world`). The `ReplayCursor` stands in for the model-call results
/// ALONE (`replay::is_model_call_result`), gated on the re-emitted `CallModel`
/// re-hashing to the recorded `Fingerprint` (Inv 7); every OTHER recorded input — the
/// exogenous free variables AND the agent `set_value` `ToolReturned`, whose
/// RunTool-dispatch replay is a later milestone — is re-applied directly by the loop. A
/// `Diverged` outcome is a replay failure.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(
        genesis_from_log(events, model)?,
        events,
        replay::is_model_call_result,
    )
}

// ---------------------------------------------------------------------------
// Event builders — keep the hand-authored log readable
// ---------------------------------------------------------------------------

fn session_started() -> Event {
    Event {
        origin: Origin::System,
        edge: EDGE_OPERATING,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed: SEED,
            surface_tools: Vec::new(),
        },
    }
}

/// A human direct UI manipulation (B8) — the ONLY `SurfaceMutated` ever logged,
/// always human-origin (Inv 18). Feeds the mode fold ⇒ a human actor on `edge`.
fn human_mutated(edge: u32, at: u64, op: SurfaceOp) -> Event {
    Event {
        origin: Origin::Human,
        edge,
        at,
        wall: None,
        input: LogicalInput::SurfaceMutated { op },
    }
}

/// A perceived-state refresh. `System`-origin so it is mode-NEUTRAL (it refreshes
/// `Resources.surfaces`, never the entity's Activity nor the mode fold).
fn observed(edge: u32, at: u64, surface: u32, version: u64, ax: &str, focus: Option<u32>) -> Event {
    Event {
        origin: Origin::System,
        edge,
        at,
        wall: None,
        input: LogicalInput::SurfaceObserved {
            surface,
            version,
            ax_digest: Hash(ax.into()),
            focus,
            selection: focus.map(|_| Selection("range:0-0".into())),
            viewport: Viewport(String::new()),
            window: WindowState(String::new()),
            cursor: None,
        },
    }
}

fn user_message(edge: u32, at: u64, text: &str) -> Event {
    Event {
        origin: Origin::Human,
        edge,
        at,
        wall: None,
        input: LogicalInput::UserMessage {
            to: 0,
            text: text.into(),
        },
    }
}

// ---------------------------------------------------------------------------
// The hand-authored mixed-mode log
// ---------------------------------------------------------------------------

/// Build the synthetic session. Three edges fold to three different modes
/// simultaneously, an interleaved `SurfaceObserved` stream refreshes perception,
/// and the agent's sole write is one `ToolReturned`. The recorded
/// `ModelResponded` carries the fingerprint the re-emitted `CallModel` re-hashes
/// to (a one-message user History, the empty P0 ToolSet, the world-default
/// params), so the replay reuses it instead of diverging.
fn authored_log(model: &ModelConfig) -> Vec<Event> {
    // The continuation request Intake re-emits for the `please tidy the form`
    // turn: a fresh entity's History is exactly that one user message.
    let request_history = History(vec![Msg {
        role: Role::User,
        content: vec![Block::Text {
            text: "please tidy the form".into(),
        }],
    }]);
    let fingerprint = rubberdux::agent::world::effects::fingerprint_call(
        &request_history,
        &rubberdux::agent::world::effects::ToolSet::default(),
        model,
    )
    .expect("compute the recorded request fingerprint");

    // The agent's UI write rides INSIDE its `ToolReturned` as the surface_ops
    // projection envelope — its sole log record (Inv 18). No separate
    // `SurfaceMutated` is authored for it.
    let agent_write_envelope = serde_json::json!({
        "surface_ops": [
            {
                "op": "set_value",
                "surface": SURFACE_AGENT,
                "element": 7,
                "value": "agent typed",
                "base_version": null
            }
        ]
    })
    .to_string();

    vec![
        // tick 0 — session header (System, neutral).
        session_started(),
        // tick 1 — perceive surface S2 at version 5 BEFORE the agent writes it
        // (an interleaved, mode-neutral observation on the Driven edge).
        observed(EDGE_DRIVEN, 1, SURFACE_AGENT, 5, "ax-s2-v5", Some(7)),
        // ticks 2–3 — the human grabs the wheel on S0 twice (edge 0 ⇒ Operating).
        human_mutated(
            EDGE_OPERATING,
            2,
            SurfaceOp::SetValue {
                surface: SURFACE_HUMAN,
                element: 0,
                value: serde_json::json!("alpha"),
                base_version: None,
            },
        ),
        human_mutated(
            EDGE_OPERATING,
            3,
            SurfaceOp::SetValue {
                surface: SURFACE_HUMAN,
                element: 0,
                value: serde_json::json!("beta"),
                base_version: None,
            },
        ),
        // tick 4 — a human click on the shared surface S1 (edge 1, human actor).
        human_mutated(
            EDGE_ASSISTED,
            4,
            SurfaceOp::Click {
                surface: SURFACE_SHARED,
                element: 3,
                point: None,
                base_version: None,
            },
        ),
        // tick 5 — perceive S1 at version 1 (interleaved, mode-neutral refresh).
        observed(EDGE_ASSISTED, 5, SURFACE_SHARED, 1, "ax-s1-v1", Some(3)),
        // tick 6 — the human asks the agent for help on the shared edge (edge 1,
        // human actor) — drives the one model turn the replay cursor stands in.
        user_message(EDGE_ASSISTED, 6, "please tidy the form"),
        // tick 7 — the agent answers (edge 1, agent actor ⇒ edge 1 is Assisted).
        Event {
            origin: Origin::Agent,
            edge: EDGE_ASSISTED,
            at: 7,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd: 0,
                entity: 0,
                fingerprint,
                blocks: vec![Block::Text { text: "on it".into() }],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: model.model.clone(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        },
        // tick 8 — the agent writes S2 via `set_value`, recorded ONCE as this
        // `ToolReturned` (edge 2, agent actor ⇒ edge 2 is Driven; Inv 18).
        Event {
            origin: Origin::Agent,
            edge: EDGE_DRIVEN,
            at: 8,
            wall: None,
            input: LogicalInput::ToolReturned {
                cmd: 1,
                entity: 0,
                fingerprint: rubberdux::agent::world::inputs::Fingerprint("fp-ui-set-value".into()),
                result: vec![Block::ToolResult {
                    tool_use_id: "tu_set_value".into(),
                    content: vec![Block::Text {
                        text: agent_write_envelope,
                    }],
                    is_error: false,
                }],
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// VC-U.5 — surface view + per-edge mode + one-fact agent write + replay
// ---------------------------------------------------------------------------

/// **VC-U.5** the U-workstream replay GATE. Folds the hand-authored mixed-mode log
/// and asserts (1) the surface view, (2) per-edge mode, (3) the agent write is one
/// fact (Inv 18), then (4) replays the log to a byte-identical World with zero
/// model/tool calls (Inv 6). No macOS, no live model.
#[test]
fn vc_u_5_mixed_mode_log_folds_and_replays_byte_identical() {
    let model = offline_model();
    let events = authored_log(&model);

    // The canonical ("live") World: re-apply every recorded Event through `tick`.
    let live = fold_log(&events, &model).expect("the synthetic log folds");

    // --- (1) Surface view: per-surface state/version --------------------------

    // S0 — human-only, never observed: two `SetValue` bumps ⇒ version 2, the
    // opaque perception fields still empty (unobserved).
    let s0 = live
        .resources
        .surfaces
        .get(&SURFACE_HUMAN)
        .expect("S0 present after human writes");
    assert_eq!(s0.version, 2, "two human SetValue ops bump S0 0→1→2");
    assert_eq!(s0.ax_digest, Hash(String::new()), "S0 was never observed");

    // S1 — human click then a perception refresh at version 1: the observation
    // overwrote perception, and no later op bumped it.
    let s1 = live
        .resources
        .surfaces
        .get(&SURFACE_SHARED)
        .expect("S1 present");
    assert_eq!(s1.version, 1, "the human click bumped S1 0→1, observed at 1");
    assert_eq!(s1.ax_digest, Hash("ax-s1-v1".into()), "S1 carries the observed digest");
    assert_eq!(s1.focus, Some(3), "S1 carries the observed focus");

    // S2 — observed at version 5, then the agent `set_value` bumps it ONCE: 5→6
    // (NOT 7 — proof the projection folded exactly once, never double-applied).
    let s2 = live
        .resources
        .surfaces
        .get(&SURFACE_AGENT)
        .expect("S2 present");
    assert_eq!(
        s2.version, 6,
        "the agent write bumps the observed S2 5→6 exactly once (Inv 18)"
    );
    assert_eq!(
        s2.ax_digest,
        Hash("ax-s2-v5".into()),
        "the agent op bumps version but preserves the observed perception"
    );

    // --- (2) Mode per edge (Inv 19) -------------------------------------------

    assert_eq!(
        mode(&events, EDGE_OPERATING, WINDOW),
        Mode::Operating,
        "a human-only edge folds to Operating"
    );
    assert_eq!(
        mode(&events, EDGE_ASSISTED, WINDOW),
        Mode::Assisted,
        "an edge where Human and Agent interleave folds to Assisted"
    );
    assert_eq!(
        mode(&events, EDGE_DRIVEN, WINDOW),
        Mode::Driven,
        "an Agent-only edge (the neutral observation aside) folds to Driven"
    );

    // --- (3) The agent write is one fact (Inv 18) -----------------------------

    // EVERY `SurfaceMutated` in the log is human-origin; there is NO agent-origin
    // `SurfaceMutated` at all (the agent never re-authors its write as one).
    let surface_mutated: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::SurfaceMutated { .. }))
        .collect();
    assert!(
        !surface_mutated.is_empty(),
        "the session has human SurfaceMutated events"
    );
    assert!(
        surface_mutated.iter().all(|e| e.origin == Origin::Human),
        "every SurfaceMutated is human-origin (Inv 18)"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.origin == Origin::Agent
                && matches!(e.input, LogicalInput::SurfaceMutated { .. }))
            .count(),
        0,
        "there is NO agent-origin SurfaceMutated record (Inv 18)"
    );

    // The agent UI write is recorded EXACTLY ONCE — as a single agent-origin
    // `ToolReturned` carrying the surface_ops projection envelope.
    let tool_returned: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::ToolReturned { .. }))
        .collect();
    assert_eq!(
        tool_returned.len(),
        1,
        "the agent write is recorded exactly once as a ToolReturned (Inv 18)"
    );
    let agent_write = tool_returned[0];
    assert_eq!(agent_write.origin, Origin::Agent, "the write is agent-origin");
    if let LogicalInput::ToolReturned { result, .. } = &agent_write.input {
        let ops = surface_ops_from_tool_result(result);
        assert_eq!(
            ops.len(),
            1,
            "the ToolReturned carries the single set_value the surface view projects"
        );
        assert_eq!(ops[0].surface(), SURFACE_AGENT, "the write targets S2");
    } else {
        panic!("expected the ToolReturned variant");
    }

    // The model turn settled the entity, leaving user + assistant text (the model
    // turn really folded, so the Assisted edge has a genuine agent answer).
    let entity = live.entities.get(&0).expect("primary entity present");
    assert!(matches!(entity.activity, Activity::Idle), "the model turn settled to Idle");
    assert_eq!(entity.history.0.len(), 2, "History holds user + assistant");
    assert!(
        matches!(
            &entity.history.0[1],
            Msg { role: Role::Assistant, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == "on it")
        ),
        "the recorded assistant answer settled into History"
    );

    // --- (4) Replay determinism (Inv 6) ---------------------------------------

    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let replay = replay_log(&events, &model).expect("replay folds the recorded log");

    // Byte-identical World by canonical serialization (Inv 6): replay over the
    // cursor-driven path reconstructs the same World the live fold built, even
    // though the model result arrived via the fingerprint-gated cursor.
    let live_bytes = serde_json::to_vec(&live).expect("serialize live World");
    let replay_bytes = serde_json::to_vec(&replay).expect("serialize replay World");
    assert_eq!(
        live_bytes, replay_bytes,
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "replay must reconstruct an equal World");

    // Folding is deterministic: two independent replays yield identical bytes.
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
