//! VC-1.1 — the tool loop as a hand-authored synthetic log that folds through the
//! pure `tick` reducer and then REPLAYS to a byte-identical `World` with zero
//! model/tool re-invocation (the Phase-1a determinism core).
//!
//! This file is pure composition — it touches no `src/agent/world/*` file. It wires
//! the genesis `World`, the pure `tick` reducer, and the REPLAY driver
//! (`drive_replay` + `ReplayCursor`) into one loop over a hand-authored log, then
//! proves:
//!
//! - **Tool loop (VC-1.1)** — a `ModelResponded·ToolUse` with N tool_use blocks
//!   branches into `ResolvingToolUses` with N `Local` slots; ToolSystem emits one
//!   `RunTool` per slot; each `ToolReturned` resolves its slot BY `cmd` (not arrival
//!   order); once EVERY slot is `Done` exactly ONE user `Msg` is assembled with the
//!   results in ascending `ordinal` order, the entity continues `→ Thinking`, and a
//!   final `ModelResponded·EndTurn` settles it `→ Idle`. (Inv 16, 10.)
//! - **Replay determinism (VC-1.5 / Inv 6, 7, 9)** — replaying the same log under the
//!   replay driver — with a `ModelCaller` that PANICS if invoked — reconstructs a
//!   `World` byte-identical to the live fold with ZERO model/tool re-invocation.
//!
//! It makes NO live model call (a pure fold + replay of a hand-authored log), so it
//! needs no credentials and runs on every developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), 7 (fingerprint),
//! 9 (log = single source of truth), 10 (totality), 16 (slot identity).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::{Command, ModelCaller, fingerprint_call};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, SlotKind,
    SlotState, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8). Inert here — no System
// draws — but it must reseed identically for the two folds to match.
const SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Genesis — the World construction no System performs from `SessionStarted`
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary entity at `Idle`,
/// and `Resources` seeded from the session's `seed`. The shell owns genesis (no P1a
/// System creates the entity from `SessionStarted`), and BOTH the live fold and the
/// replay reconstruct it the SAME way — which is what makes the two Worlds comparable
/// byte-for-byte. Mirrors the walking-skeleton genesis with the CURRENT Components shape.
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

/// Rebuild genesis from the recorded log: the seed is the one hidden input that must
/// cross a recorded boundary, so replay reseeds `Rng` from the log's `SessionStarted`
/// header rather than from any live source.
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

/// The world-default `ModelConfig`. Its `model` id rides in the request the re-emitted
/// `CallModel` fingerprints, so the recorded `ModelResponded` results are stamped with a
/// fingerprint computed against THIS exact value; it makes no real call.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-tool-loop-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// ExplodingClient — the runtime witness that replay never reaches a model client
// ---------------------------------------------------------------------------

/// A `ModelCaller` that records any invocation and then panics. `drive_replay` takes NO
/// client, so this can never be threaded into `replay_log` by construction; constructing
/// it and asserting its counter stays zero makes the "zero model/tool calls" guarantee
/// (Inv 6) explicit at runtime, while the panic is the structural backstop.
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

/// Fold the WHOLE recorded log through the pure `tick` reducer, event by event. Every
/// input — the exogenous free variables and the already-recorded DERIVED results — is
/// present in the log, so there is nothing to dispatch and the emitted Commands are
/// discarded. This is the canonical ("live") World the replay must reproduce byte-for-byte.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    Ok(replay::fold_log(genesis_from_log(events, model)?, events))
}

// ---------------------------------------------------------------------------
// replay_log — the REPLAY driver: stand in model-call results from the cursor
// ---------------------------------------------------------------------------

/// Fold the SAME recorded log from genesis under the REPLAY driver (the promoted
/// `replay::replay_world`). The `ReplayCursor` stands in for the model-call results
/// ALONE (`replay::is_model_call_result`): in a tool loop the `ToolReturned` results
/// are re-applied directly by the loop, so a continuation `CallModel` re-emitted after
/// them pulls the NEXT `ModelResponded` from the cursor — never a `ToolReturned`. A
/// `Diverged` outcome is a replay failure.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(
        genesis_from_log(events, model)?,
        events,
        replay::is_model_call_result,
    )
}

// ---------------------------------------------------------------------------
// Fingerprint stamping — reproduce what the LIVE driver records on each result
// ---------------------------------------------------------------------------

/// Stamp each model-call result's `fingerprint` with the value the LIVE driver would
/// have recorded: the hash of the request the reducer re-emits for that `cmd`. We dry-
/// fold the log (the result fingerprints are inert to the fold — no System branches on
/// them) and, for every emitted `CallModel`, hash its `(messages, tools, params)` exactly
/// as `drive_live`/`drive_replay` do, then write that hash onto the result that answers
/// the same `cmd`. This makes the recorded fingerprints content-correct by construction,
/// so the replay reuses each result instead of diverging (Inv 7) — without hand-authoring
/// any continuation `History`.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig) {
    let mut world = genesis_from_log(events, model).expect("genesis for fingerprint pass");
    let mut by_cmd: BTreeMap<CmdId, Fingerprint> = BTreeMap::new();
    for ev in events.iter() {
        let (next, commands) = tick(&world, ev);
        world = next;
        for command in &commands {
            if let Command::CallModel {
                cmd,
                messages,
                tools,
                params,
                ..
            } = command
            {
                let fp = fingerprint_call(messages, tools, params).expect("fingerprint a request");
                by_cmd.insert(*cmd, fp);
            }
        }
    }
    for ev in events.iter_mut() {
        match &mut ev.input {
            LogicalInput::ModelResponded {
                cmd, fingerprint, ..
            }
            | LogicalInput::ModelFailed {
                cmd, fingerprint, ..
            }
            | LogicalInput::InferenceCancelled {
                cmd, fingerprint, ..
            } => {
                if let Some(fp) = by_cmd.get(cmd) {
                    *fingerprint = fp.clone();
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Event builders — keep the hand-authored log readable
// ---------------------------------------------------------------------------

fn session_started() -> Event {
    Event {
        origin: Origin::System,
        edge: 0,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed: SEED,
            surface_tools: Vec::new(),
        },
    }
}

fn user_message(at: u64, text: &str) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::UserMessage {
            to: 0,
            text: text.into(),
        },
    }
}

/// A `ModelResponded` (fingerprint left as a placeholder; `stamp_fingerprints` fills it).
fn model_responded(at: u64, cmd: CmdId, blocks: Vec<Block>, stop: StopReason) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            blocks,
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-tool-loop-replay".into(),
                stop_reason: stop,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

fn tool_returned(at: u64, cmd: CmdId, result: Vec<Block>) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ToolReturned {
            cmd,
            entity: 0,
            // A `ToolReturned` is re-applied directly by the replay loop and its result
            // is never gated on this fingerprint (the RunTool-dispatch replay is a later
            // milestone), so a placeholder is sufficient here.
            fingerprint: Fingerprint("fp-tool-result".into()),
            result,
        },
    }
}

fn tool_use_block(id: &str, name: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

// ---------------------------------------------------------------------------
// The hand-authored tool-loop log
// ---------------------------------------------------------------------------

/// Build the synthetic tool-loop session: a two-tool-use assistant turn, the two tool
/// results arriving in REVERSE ordinal order (B before A, to prove `cmd`-matching, Inv
/// 16), the batched continuation, and a final `EndTurn`. The fingerprints are stamped to
/// the exact requests the reducer re-emits.
fn authored_tool_loop_log(model: &ModelConfig) -> Vec<Event> {
    // The assistant turn's two tool_use blocks (ordinals 0 and 1).
    let tool_uses = vec![tool_use_block("tu_a", "tool_a"), tool_use_block("tu_b", "tool_b")];

    let mut events = vec![
        session_started(),
        user_message(1, "run both tools"),
        // ModelResponded·ToolUse answers the intake cmd (0): branches into two Local slots.
        model_responded(2, 0, tool_uses, StopReason::ToolUse),
        // The two tool results arrive in REVERSE ordinal order: tu_b (cmd 2) settles
        // first, tu_a (cmd 1) second — proving results are matched by `cmd`, not arrival.
        tool_returned(3, 2, vec![Block::Text { text: "result B".into() }]),
        tool_returned(4, 1, vec![Block::Text { text: "result A".into() }]),
        // The continuation answers the new turn cmd (3) with EndTurn → Idle.
        model_responded(
            5,
            3,
            vec![Block::Text {
                text: "all done".into(),
            }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, model);
    events
}

// ---------------------------------------------------------------------------
// VC-1.1 + VC-1.5 — tool loop folds, then replays byte-identical
// ---------------------------------------------------------------------------

/// **VC-1.1** the tool loop: N `RunTool`s, results matched by `cmd`, ONE batched user
/// `Msg` in ascending `ordinal` order, continuation → `EndTurn` → `Idle`. **VC-1.5** then
/// replays the SAME log to a byte-identical `World` with the model client invoked zero
/// times (Inv 6). No macOS, no live model.
#[test]
fn vc_1_1_tool_loop_batches_results_and_replays_byte_identical() {
    let model = offline_model();
    let events = authored_tool_loop_log(&model);

    // --- (1) Slot resolution: the ToolUse turn emits ONE RunTool per Local slot -------
    // Fold up to and including the `ModelResponded·ToolUse` tick and inspect that tick's
    // Commands and the resulting slot shape.
    let mut world = genesis_from_log(&events, &model).expect("genesis");
    let mut run_tool_count = 0usize;
    for ev in &events[..3] {
        let (next, commands) = tick(&world, ev);
        world = next;
        run_tool_count += commands
            .iter()
            .filter(|c| matches!(c, Command::RunTool { .. }))
            .count();
    }
    assert_eq!(
        run_tool_count, 2,
        "the ToolUse turn emits exactly one RunTool per Local slot (N = 2)"
    );
    match &world.entities.get(&0).expect("entity").activity {
        Activity::ResolvingToolUses { slots } => {
            assert_eq!(slots.len(), 2, "one Local slot per tool_use block");
            assert_eq!(
                (slots[0].tool_use_id.as_str(), slots[0].ordinal),
                ("tu_a", 0)
            );
            assert_eq!(
                (slots[1].tool_use_id.as_str(), slots[1].ordinal),
                ("tu_b", 1)
            );
            assert!(slots.iter().all(|s| matches!(s.kind, SlotKind::Local)));
            assert!(
                slots
                    .iter()
                    .all(|s| matches!(s.state, SlotState::Pending { cmd: Some(_) })),
                "each slot records the minted cmd that resolves it (Inv 16)"
            );
        }
        other => panic!("expected ResolvingToolUses, got {other:?}"),
    }

    // --- (2) The canonical ("live") World: re-apply every recorded Event via `tick` ----
    let live = fold_log(&events, &model).expect("the tool-loop log folds");
    let e = live.entities.get(&0).expect("primary entity present");
    assert!(
        matches!(e.activity, Activity::Idle),
        "the tool loop settles to Idle"
    );

    // History: user, assistant(tool_use a,b), ONE user(tool_result a,b), assistant(final).
    assert_eq!(
        e.history.0.len(),
        4,
        "user + assistant(tool_use) + one batched user(tool_result) + assistant(final)"
    );
    let batched = &e.history.0[2];
    assert!(
        matches!(batched.role, Role::User),
        "the tool results ride in ONE user Msg"
    );
    let ids: Vec<&str> = batched
        .content
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["tu_a", "tu_b"],
        "the batched message holds every result in ASCENDING ordinal order (NOT arrival \
         order — tu_b's result was recorded first)"
    );
    assert_eq!(batched.content.len(), 2, "exactly two tool_results, batched once");
    assert!(
        matches!(
            &e.history.0[3],
            Msg { role: Role::Assistant, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == "all done")
        ),
        "the continuation's EndTurn answer settled into History"
    );

    // --- (3) Replay determinism (Inv 6) -----------------------------------------------
    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let replay = replay_log(&events, &model).expect("replay folds the recorded tool-loop log");

    // Byte-identical World by canonical serialization (Inv 6/7/9).
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
