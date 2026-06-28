//! The Phase-0 walking skeleton: compose the whole `src/agent/world/` stack into
//! a runnable loop and prove the milestone's first end-to-end claims.
//!
//! This file is pure composition — it touches no `src/agent/world/*` file. It
//! wires the genesis `World`, the pure `tick` reducer, the append-only
//! `EventLog`, and the TWO drivers (`drive_live` / `drive_replay`) into one loop,
//! then asserts the design's "TWO-DRIVER REPLAY" property: the Systems are
//! identical under both drivers and ONLY the driver differs, so a recorded log
//! replays to a byte-identical `World` with ZERO model calls.
//!
//! - **VC-0.1** [Happy, live; gated] one real `UserMessage` turn records
//!   `SessionStarted` + `UserMessage` + `ModelResponded`, ends `Idle`, and leaves
//!   the assistant text in `History`. (Inv 2, 12.)
//! - **VC-0.2** [Edge] replaying that live log under the replay driver — with an
//!   exploding `ModelCaller` that the driver structurally cannot reach —
//!   reconstructs a byte-identical `World` and invokes the client zero times.
//!   (Inv 6, 7, 9.)
//! - **VC-0.3** [offline backstop] a hand-authored synthetic log folds
//!   deterministically (two folds → identical canonical digests) with no network
//!   or credentials. (Inv 6, 9.)
//!
//! See `docs/agent/world/ecs-runtime.md` (Tick discipline, TWO-DRIVER REPLAY).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::{
    Command, ModelCaller, ResultStamp, SurfaceDriver, ToolSet, drive_live, fingerprint_call,
};
use rubberdux::agent::world::surface::SurfaceView;
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::inputs::{
    Capabilities, Event, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
};
use rubberdux::agent::world::model_client::MessagesClient;
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Lineage, ModelConfig, Resources, Tick, World,
};
use rubberdux::error::Error;

use crate::support::live_gate::skip_without_live_llm;

// ---------------------------------------------------------------------------
// Genesis — the World construction no System performs from `SessionStarted`
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary entity at
/// `Idle`, and `Resources` seeded from the session's `seed`. No P0 System creates
/// this entity from `SessionStarted` (Intake only admits a `UserMessage`), so the
/// shell owns genesis — and both the live run and the replay reconstruct it the
/// SAME way, which is what makes the two Worlds comparable byte-for-byte.
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

/// Rebuild genesis from a recorded log: the seed is the one hidden input that
/// must cross a recorded boundary, so replay reseeds `Rng` from the log's
/// `SessionStarted` rather than from any live source. The world-default
/// `ModelConfig` is not recorded in P0, so the caller supplies the SAME value the
/// live run used (documented genesis-parity, not a live re-resolution).
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

// ---------------------------------------------------------------------------
// run_live — compose `tick` + the LIVE driver into a real turn loop
// ---------------------------------------------------------------------------

/// Seed the log with `SessionStarted` + `UserMessage` (folding each via `tick`),
/// then drain: whenever a tick emits Commands, the LIVE driver performs the real
/// effect, APPENDS the result `Event` (log-before-apply), and we fold it back —
/// re-entering the drain for any continuation — until the entity is `Idle` with
/// no pending Commands. Returns the final `World` and the populated log.
/// A no-op surface-drive sink: the walking skeleton drives only `CallModel` turns
/// (no `set_value` RunTool), so the driver is never reached — it stands in for the
/// injected sink so `drive_live` type-checks.
struct NoSurfaceDrive;
impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}

async fn run_live<C: ModelCaller>(
    client: &C,
    model: &ModelConfig,
    seed: u64,
    user_text: &str,
) -> Result<(World, MemoryEventLog), Error> {
    let mut log = MemoryEventLog::new();
    let mut world = genesis(seed, model);

    let session = Event {
        origin: Origin::System,
        edge: 0,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed,
            surface_tools: Vec::new(),
        },
    };
    let user = Event {
        origin: Origin::Human,
        edge: 0,
        at: 1,
        wall: None,
        input: LogicalInput::UserMessage {
            to: 0,
            text: user_text.into(),
        },
    };

    // Result Events are recorded after the two seed events (ticks 0 and 1).
    let mut next_at: Tick = 2;

    for ev in [&session, &user] {
        // Log-before-apply (Inv 4): durable BEFORE the reducer folds it.
        log.append(ev)?;
        let (next, mut commands) = tick(&world, ev);
        world = next;

        while !commands.is_empty() {
            let stamp = ResultStamp {
                edge: 0,
                app_edge: 1,
                at: next_at,
                wall: None,
            };
            next_at += 1;
            // LIVE driver: real model call, appends the result Event to the log.
            let results = drive_live(
                &commands,
                stamp,
                &SurfaceView::new(),
                client,
                &NoSurfaceDrive,
                &mut log,
            )
            .await?;
            commands = Vec::new();
            for result in &results {
                let (next, mut cmds) = tick(&world, result);
                world = next;
                commands.append(&mut cmds);
            }
        }
    }

    Ok((world, log))
}

// ---------------------------------------------------------------------------
// run_replay — compose `tick` + the REPLAY driver (no client) over the log
// ---------------------------------------------------------------------------

/// Fold the SAME recorded log from genesis under the REPLAY driver. The loop is
/// the live loop with the driver swapped: it folds the exogenous events, and for
/// each tick's Commands it calls `drive_replay` — which takes NO `ModelCaller`
/// and so cannot call the model by construction — standing in the already-logged
/// result. A `Diverged` outcome means the re-emitted request stopped matching the
/// record, which a faithful replay never does, so it is reported as an error.
fn run_replay(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(genesis_from_log(events, model)?, events, |input| {
        !replay::is_exogenous(input)
    })
}

// ---------------------------------------------------------------------------
// ExplodingClient — the guarantee that replay never reaches a model client
// ---------------------------------------------------------------------------

/// A `ModelCaller` that records any invocation and then panics. The replay driver
/// takes NO client, so this can never be threaded into `run_replay`; constructing
/// it and asserting its counter stays zero makes the "zero model calls" guarantee
/// explicit at runtime, while the panic is the structural backstop.
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
// Shared fixtures
// ---------------------------------------------------------------------------

/// The world-default `ModelConfig` for the OFFLINE backstop. Its `model` id is
/// inert because VC-0.3 makes no real call.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-walking-skeleton".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// The world-default `ModelConfig` for the LIVE turn. The model id rides in the
/// `/v1/messages` request body (`MessageBuilder` reads `params.model`), so it is
/// taken from `RUBBERDUX_LLM_MODEL` — the same knob `MessagesClient::from_env`
/// uses — defaulting to the Anthropic default so the gate and the call agree.
fn live_model() -> ModelConfig {
    let model = std::env::var("RUBBERDUX_LLM_MODEL")
        .unwrap_or_else(|_| "claude-opus-4-5-20251101".into());
    ModelConfig {
        model,
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// VC-0.1 + VC-0.2 — live record, then byte-identical replay (gated)
// ---------------------------------------------------------------------------

/// **VC-0.1** drives one real turn and asserts the recorded log and History, then
/// **VC-0.2** replays that exact log under the replay driver and asserts a
/// byte-identical `World` with zero client calls. They share one test because
/// VC-0.2 replays "the VC-0.1 log"; the gate skips both without live credentials.
#[tokio::test]
async fn vc_0_1_live_turn_records_then_vc_0_2_replays_byte_identical() {
    let test = "vc_0_1_live_turn_records_then_vc_0_2_replays_byte_identical";
    if skip_without_live_llm(test) {
        return;
    }

    let model = live_model();
    let seed: u64 = 42;
    let client = MessagesClient::from_env().expect("build MessagesClient from env");

    // --- VC-0.1: one real turn ------------------------------------------------
    let (live_world, log) = run_live(&client, &model, seed, "Reply with exactly one word: pong")
        .await
        .expect("the live turn runs to completion");

    let events = log.load().expect("load the recorded log");

    // The log accrued SessionStarted (first) + UserMessage + ModelResponded, and
    // the live turn must have SUCCEEDED (no ModelFailed).
    assert!(
        matches!(
            events.first().map(|e| &e.input),
            Some(LogicalInput::SessionStarted { .. })
        ),
        "the log opens with SessionStarted"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.input, LogicalInput::UserMessage { .. })),
        "the log records the UserMessage"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelResponded { .. })),
        "a real model turn records a ModelResponded"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelFailed { .. })),
        "the live turn must succeed (no ModelFailed)"
    );

    // The final Activity is Idle and History holds non-empty assistant text.
    let entity = live_world.entities.get(&0).expect("primary entity present");
    assert!(
        matches!(entity.activity, Activity::Idle),
        "the turn returns Thinking -> Idle"
    );
    let assistant = entity
        .history
        .0
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant))
        .expect("an assistant message is recorded in History");
    assert!(
        assistant
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if !text.is_empty())),
        "the assistant message holds non-empty text"
    );

    // --- VC-0.2: replay the live log, zero model calls ------------------------
    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `run_replay`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let replay_world = run_replay(&events, &model).expect("replay folds the recorded log");

    // Byte-identical World by canonical serialization (Inv 6/7/9).
    let live_bytes = serde_json::to_vec(&live_world).expect("serialize live World");
    let replay_bytes = serde_json::to_vec(&replay_world).expect("serialize replay World");
    assert_eq!(
        live_bytes, replay_bytes,
        "replay must reconstruct a byte-identical World"
    );
    assert_eq!(
        live_world, replay_world,
        "replay must reconstruct an equal World"
    );

    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "replay must invoke the model client zero times"
    );
}

// ---------------------------------------------------------------------------
// VC-0.3 — offline synthetic-log fold determinism (no credentials)
// ---------------------------------------------------------------------------

/// **VC-0.3** the offline backstop: a hand-authored `SessionStarted` +
/// `UserMessage` + fabricated `ModelResponded(EndTurn)` log folds deterministically
/// under the replay driver — two folds yield byte-identical canonical digests —
/// and reconstructs the expected History with the model client invoked zero times.
/// Requires no network or credentials.
#[test]
fn vc_0_3_synthetic_log_replays_deterministically_offline() {
    let model = offline_model();
    let seed: u64 = 7;
    let user_text = "ping";
    let assistant_text = "pong";

    // The fabricated result must carry the request `Fingerprint` the re-emitted
    // `CallModel` re-hashes to, or the replay driver would treat it as diverged.
    // Intake re-emits exactly this request: the one-message user History, the
    // empty P0 ToolSet, and the world-default params.
    let request_history = History(vec![Msg {
        role: Role::User,
        content: vec![Block::Text {
            text: user_text.into(),
        }],
    }]);
    let fingerprint = fingerprint_call(&request_history, &ToolSet::default(), &model)
        .expect("compute the recorded request fingerprint");

    let events = vec![
        Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input: LogicalInput::SessionStarted {
            seed,
            surface_tools: Vec::new(),
        },
        },
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
    ];

    // Two independent folds of the SAME log must produce identical canonical bytes.
    let world_a = run_replay(&events, &model).expect("first replay fold");
    let world_b = run_replay(&events, &model).expect("second replay fold");
    let digest_a = serde_json::to_vec(&world_a).expect("serialize first fold");
    let digest_b = serde_json::to_vec(&world_b).expect("serialize second fold");
    assert_eq!(
        digest_a, digest_b,
        "folding a synthetic log is deterministic"
    );

    // Structural correctness: the turn settled to Idle with user + assistant text.
    let entity = world_a.entities.get(&0).expect("primary entity present");
    assert!(
        matches!(entity.activity, Activity::Idle),
        "the synthetic turn settles to Idle"
    );
    assert_eq!(entity.history.0.len(), 2, "History holds user + assistant");
    assert!(
        matches!(
            &entity.history.0[0],
            Msg { role: Role::User, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == user_text)
        ),
        "the user message is recorded first"
    );
    assert!(
        matches!(
            &entity.history.0[1],
            Msg { role: Role::Assistant, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == assistant_text)
        ),
        "the fabricated assistant text settles into History"
    );

    // The offline backstop for VC-0.2's zero-call guarantee: `run_replay` takes no
    // client, so this exploding stand-in is never reached.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let _ = run_replay(&events, &model).expect("replay never touches a model client");
    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "offline replay invokes the model client zero times"
    );
}
