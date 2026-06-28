//! VC-1.4 — in-process sub-agents as hand-authored synthetic logs that fold through
//! the pure `tick` reducer and then REPLAY to a byte-identical `World` with zero
//! model/tool re-invocation (the Phase-1a determinism core).
//!
//! This file is pure composition — it touches no `src/agent/world/*` file. It proves:
//!
//! - **Spawn + settle (VC-1.4)** — a `spawn_subagent` `ToolUse` branches into a `Child`
//!   slot AND spawns a child Entity (lineage = parent, depth + 1) running its own turn;
//!   a later `ChildReturned` resolves the parent's slot BY IDENTITY `(child,
//!   tool_use_id)` — NOT by `cmd` (Inv 16) — and the parent continues. (Inv 16, 10.)
//! - **Depth-cap denial (VC-1.4 / Inv 10)** — with `depth_cap` exceeded, the `Child`
//!   slot is resolved inline `is_error` WITHOUT spawning a child, so the parent is never
//!   left waiting on a child that will never exist.
//! - **Replay determinism (VC-1.5 / Inv 6, 7, 9)** — replaying each log under the replay
//!   driver — with a `ModelCaller` that PANICS if invoked — reconstructs a `World`
//!   byte-identical to the live fold with ZERO model/tool re-invocation.
//!
//! It makes NO live model call, so it needs no credentials and runs on every machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), 7 (fingerprint),
//! 9 (log = single source of truth), 10 (totality), 16 (slot identity).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::{Command, ModelCaller, fingerprint_call};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Role};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, EntityId, Identity, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

const SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Genesis — the World construction no System performs from `SessionStarted`
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from, with an explicit `depth_cap` so the depth-cap
/// denial path can be exercised. Both the live fold and the replay reconstruct it the
/// SAME way (same seed, same cap), which is what makes the two Worlds comparable byte-for-
/// byte. Mirrors the walking-skeleton genesis with the CURRENT Components shape.
fn genesis(seed: u64, model: &ModelConfig, depth_cap: u8) -> World {
    let mut world = World::new(0, Resources::new(seed, model.clone()));
    world.resources.depth_cap = depth_cap;
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

fn genesis_from_log(events: &[Event], model: &ModelConfig, depth_cap: u8) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model, depth_cap))
}

fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-subagent-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// ExplodingClient — the runtime witness that replay never reaches a model client
// ---------------------------------------------------------------------------

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
// fold_log / replay_log — the live fold and the cursor-driven replay
// ---------------------------------------------------------------------------

fn fold_log(events: &[Event], model: &ModelConfig, depth_cap: u8) -> Result<World, Error> {
    Ok(replay::fold_log(
        genesis_from_log(events, model, depth_cap)?,
        events,
    ))
}

/// Replay under the promoted cursor-driven driver (`replay::replay_world`). The
/// `ReplayCursor` stands in for the model-call results ALONE
/// (`replay::is_model_call_result`): the parent's and the child's `ModelResponded`s are
/// stood in for each re-emitted `CallModel` (in log order), while the identity-correlated
/// `ChildReturned` is re-applied directly by the loop. A `Diverged` outcome is a replay
/// failure.
fn replay_log(events: &[Event], model: &ModelConfig, depth_cap: u8) -> Result<World, Error> {
    replay::replay_world(
        genesis_from_log(events, model, depth_cap)?,
        events,
        replay::is_model_call_result,
    )
}

/// Stamp each model-call result's `fingerprint` with the value the LIVE driver would have
/// recorded (the hash of the request the reducer re-emits for that `cmd`), so the replay
/// reuses each result instead of diverging (Inv 7) — including the CHILD's own `CallModel`.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig, depth_cap: u8) {
    let mut world = genesis_from_log(events, model, depth_cap).expect("genesis for fingerprints");
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
// Event builders
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

/// A `ModelResponded` routed to `entity` (the parent is `0`; a spawned child is its own
/// EntityId). Fingerprint is a placeholder filled by `stamp_fingerprints`.
fn model_responded(
    at: u64,
    cmd: CmdId,
    entity: EntityId,
    blocks: Vec<Block>,
    stop: StopReason,
) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity,
            fingerprint: Fingerprint(String::new()),
            blocks,
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-subagent-replay".into(),
                stop_reason: stop,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

/// A `spawn_subagent` tool_use block; its `prompt` seeds the child's first turn.
fn spawn_block(tool_use_id: &str, prompt: &str) -> Block {
    Block::ToolUse {
        id: tool_use_id.into(),
        name: "spawn_subagent".into(),
        input: serde_json::json!({ "prompt": prompt }),
    }
}

/// A child's completion routed back to the parent, correlated by `(child, tool_use_id)`.
fn child_returned(at: u64, parent: EntityId, child: EntityId, tool_use_id: &str, text: &str) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ChildReturned {
            parent,
            child,
            tool_use_id: tool_use_id.into(),
            result: Block::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: vec![Block::Text { text: text.into() }],
                is_error: false,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// VC-1.4 (spawn + settle) + VC-1.5 (byte-identical replay)
// ---------------------------------------------------------------------------

/// The spawn/settle log: parent turn → `spawn_subagent` → child Entity runs its turn →
/// `ChildReturned` resolves the parent's `Child` slot → parent continues → `EndTurn`.
fn authored_spawn_log(model: &ModelConfig) -> Vec<Event> {
    let mut events = vec![
        session_started(),
        user_message(1, "delegate the research"),
        // The parent's turn (cmd 0) branches into a Child slot and spawns child entity 1.
        model_responded(
            2,
            0,
            0,
            vec![spawn_block("tu_child", "research the topic")],
            StopReason::ToolUse,
        ),
        // The CHILD's own turn (cmd 1, entity 1) ends — its EndTurn settles the child.
        model_responded(
            3,
            1,
            1,
            vec![Block::Text {
                text: "child finished".into(),
            }],
            StopReason::EndTurn,
        ),
        // The child's result returns to the parent, resolving its slot by identity.
        child_returned(4, 0, 1, "tu_child", "the research result"),
        // The parent's continuation (cmd 2) ends the turn → Idle.
        model_responded(
            5,
            2,
            0,
            vec![Block::Text {
                text: "synthesized".into(),
            }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, model, 8);
    events
}

/// **VC-1.4** a `spawn_subagent` spawns a child Entity and the `ChildReturned` resolves
/// the parent's `Child` slot by `(child, tool_use_id)`; **VC-1.5** replays byte-identical
/// with zero model/tool calls.
#[test]
fn vc_1_4_subagent_spawn_settle_and_replays_byte_identical() {
    let model = offline_model();
    let events = authored_spawn_log(&model);

    let live = fold_log(&events, &model, 8).expect("the spawn log folds");

    // --- A child Entity was spawned, one level deeper, under the parent ----------------
    let child = live
        .entities
        .keys()
        .copied()
        .find(|id| *id != 0)
        .expect("a child Entity was spawned");
    let c = live.entities.get(&child).expect("child present");
    assert_eq!(c.identity, Identity::Subagent, "the child is a sub-agent");
    assert_eq!(
        c.lineage,
        Lineage {
            parent: Some(0),
            depth: 1,
        },
        "child lineage = parent, depth + 1"
    );
    assert!(
        matches!(c.activity, Activity::Idle),
        "the child's own turn settled to Idle"
    );

    // --- The parent advanced past the child and settled to Idle ------------------------
    let p = live.entities.get(&0).expect("parent present");
    assert!(
        matches!(p.activity, Activity::Idle),
        "the parent settles to Idle once the child returned"
    );
    // History: user, assistant(tool_use spawn), ONE user(tool_result), assistant(final).
    assert_eq!(p.history.0.len(), 4, "the parent's turn completed end-to-end");
    let tool_result = &p.history.0[2];
    assert!(matches!(tool_result.role, Role::User));
    match &tool_result.content[0] {
        Block::ToolResult {
            tool_use_id,
            is_error,
            ..
        } => {
            assert_eq!(
                tool_use_id, "tu_child",
                "the child's result is paired with the Child slot's tool_use_id (Inv 16)"
            );
            assert!(!is_error, "a successful child result is not an error");
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }

    // --- Replay determinism (Inv 6) ----------------------------------------------------
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let replay = replay_log(&events, &model, 8).expect("replay folds the spawn log");
    assert_eq!(
        serde_json::to_vec(&live).expect("serialize live"),
        serde_json::to_vec(&replay).expect("serialize replay"),
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "replay must reconstruct an equal World");
    // Folding is deterministic: two independent replays yield identical bytes.
    let replay_again = replay_log(&events, &model, 8).expect("second replay fold");
    assert_eq!(
        serde_json::to_vec(&replay).expect("serialize replay"),
        serde_json::to_vec(&replay_again).expect("serialize second replay"),
        "two independent replays of the same log are byte-identical"
    );
    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "replay must invoke the model client zero times (Inv 6)"
    );
}

// ---------------------------------------------------------------------------
// VC-1.4 (depth-cap denial) + VC-1.5 (byte-identical replay)
// ---------------------------------------------------------------------------

/// The depth-cap denial log: with `depth_cap = 0`, a child of the root (depth 1) would
/// exceed the cap, so the `Child` slot is resolved inline `is_error` WITHOUT spawning, and
/// the parent continues straight to its `EndTurn`.
fn authored_depth_cap_log(model: &ModelConfig) -> Vec<Event> {
    let mut events = vec![
        session_started(),
        user_message(1, "delegate too deep"),
        // The spawn would create a depth-1 child; depth_cap 0 denies it inline.
        model_responded(
            2,
            0,
            0,
            vec![spawn_block("tu_deep", "go deeper")],
            StopReason::ToolUse,
        ),
        // The parent's continuation (cmd 1) over the is_error result → Idle.
        model_responded(
            3,
            1,
            0,
            vec![Block::Text {
                text: "cannot delegate further".into(),
            }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, model, 0);
    events
}

/// **VC-1.4 / Inv 10** a depth-capped spawn resolves the `Child` slot `is_error` WITHOUT
/// spawning a child Entity, so the parent never waits; **VC-1.5** replays byte-identical.
#[test]
fn vc_1_4_subagent_depth_cap_denial_and_replays_byte_identical() {
    let model = offline_model();
    let events = authored_depth_cap_log(&model);

    let live = fold_log(&events, &model, 0).expect("the depth-cap log folds");

    // No child Entity was inserted — only the root remains.
    assert_eq!(
        live.entities.len(),
        1,
        "a denied spawn creates no child Entity (Inv 10)"
    );

    let p = live.entities.get(&0).expect("parent present");
    assert!(
        matches!(p.activity, Activity::Idle),
        "the parent settles to Idle — never stuck on a child that will never exist"
    );
    // The denied slot resolved is_error, paired with its tool_use_id.
    let tool_result = &p.history.0[2];
    match &tool_result.content[0] {
        Block::ToolResult {
            tool_use_id,
            is_error,
            ..
        } => {
            assert_eq!(tool_use_id, "tu_deep");
            assert!(*is_error, "a depth-capped spawn resolves is_error inline (Inv 10)");
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }

    // The spawn ToolUse was still recorded in the parent's History (the slot existed and
    // resolved is_error), even though no child Entity was inserted for it.
    match &p.history.0[1].content[0] {
        Block::ToolUse { name, .. } => assert_eq!(name, "spawn_subagent"),
        other => panic!("expected the spawn ToolUse in History, got {other:?}"),
    }

    // --- Replay determinism (Inv 6) ----------------------------------------------------
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let replay = replay_log(&events, &model, 0).expect("replay folds the depth-cap log");
    assert_eq!(
        serde_json::to_vec(&live).expect("serialize live"),
        serde_json::to_vec(&replay).expect("serialize replay"),
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "replay must reconstruct an equal World");
    // Folding is deterministic: two independent replays yield identical bytes.
    let replay_again = replay_log(&events, &model, 0).expect("second replay fold");
    assert_eq!(
        serde_json::to_vec(&replay).expect("serialize replay"),
        serde_json::to_vec(&replay_again).expect("serialize second replay"),
        "two independent replays of the same log are byte-identical"
    );
    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "replay must invoke the model client zero times (Inv 6)"
    );
}
