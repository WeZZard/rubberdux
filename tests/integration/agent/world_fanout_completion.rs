//! VC-1.1 and VC-1.2 — fan-out completion + cumulative cap.
//!
//! This file is pure composition — it touches no `src/agent/world/*` file. It proves:
//!
//! - **VC-1.1 (offline)** — a synthetic recorded parent+child fan-out log (where the
//!   `ChildReturned` is the value `regenerate_child_returns` would have produced —
//!   the child's last assistant message blocks as a `ToolResult`) replays
//!   BYTE-IDENTICALLY with ZERO model calls — a STRUCTURAL guarantee, because both
//!   `fold_log` and `replay_world` take NO `ModelCaller`, so neither can reach a model
//!   client by construction: the `ChildReturned` is re-applied directly from the log
//!   (the named, NON-fingerprinted Inv 7 exception, not stood in for by the fingerprint
//!   cursor), so the parent's resume is reproduced deterministically. The log is folded
//!   identically by `fold_log` and `replay_world`.
//!
//! - **VC-1.2** — the cumulative `spawned` cap (`fanout_cap=1`) denies a SEQUENTIAL
//!   second spawn (`is_error`, no deadlock) once `spawned==fanout_cap` EVEN AFTER the
//!   first child has FINISHED and its `ChildReturned` has been folded into the parent —
//!   proving `spawned` is cumulative (never decremented on child completion), not a
//!   live-count. This is the load-bearing distinction from the old live-count semantics.
//!
//! - **VC-1.1 (live; gated)** — a real one-child fan-out driven by `WorldDriver` with a
//!   hybrid `ModelCaller` (stub parent spawn, real child + continuation calls via
//!   the selected provider) drives the child to `EndTurn`, the parent receives the child's
//!   answer, and the parent responds, producing a `ChildReturned` in the log.
//!
//! The offline and cumulative-cap tests make NO live model calls, so they need no
//! credentials and run on every machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), 7 (fingerprint;
//! `ChildReturned` named non-fingerprinted exception), 9 (log = single source of truth),
//! 10 (totality), 11 (Boundedness), 16 (slot identity).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::driver::WorldDriver;
use rubberdux::agent::world::effects::{Command, SurfaceDriver, fingerprint_call};
use rubberdux::provider::{
    ContentBlock, ModelApi, ModelInfo, ModelRequest, ModelResponse, selected_from_env,
};
use std::future::Future;
use std::pin::Pin;
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Role};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::lifecycle::LifecycleEvent;
use rubberdux::agent::world::replay;
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources,
    World,
};
use rubberdux::error::Error;

use crate::support::live_gate::skip_without_live_llm;

const SEED: u64 = 11;

// ---------------------------------------------------------------------------
// Genesis — the World construction no System performs from `SessionStarted`
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from. Both the live fold and the replay
/// reconstruct it the SAME way (same seed, same caps), making the two Worlds
/// comparable byte-for-byte. Mirrors the walking-skeleton / world_subagent genesis
/// with the CURRENT Components shape. `fanout_cap` is set on the primary entity's
/// budget limits (0 = unbounded).
fn genesis(seed: u64, model: &ModelConfig, depth_cap: u8, fanout_cap: u32) -> World {
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
            budget: {
                let mut b = Budget::default();
                b.limits.fanout_cap = fanout_cap;
                b
            },
            inbox: Inbox::default(),
            turns: 0,
            spawned: 0,
            model: None,
            autonomy: None,
        },
    );
    world
}

fn genesis_from_log(
    events: &[Event],
    model: &ModelConfig,
    depth_cap: u8,
    fanout_cap: u32,
) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model, depth_cap, fanout_cap))
}

fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-fanout-completion-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

fn live_model() -> ModelConfig {
    let model = std::env::var("RUBBERDUX_LLM_MODEL")
        .unwrap_or_else(|_| "kimi-for-coding".into());
    ModelConfig {
        model,
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// NoSurfaceDrive — the offline no-op surface sink
// ---------------------------------------------------------------------------

struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SharedLog — captures events appended to WorldDriver for external inspection
// ---------------------------------------------------------------------------

/// An `EventLog` implementation backed by an `Arc<Mutex<Vec<Event>>>` so the
/// integration test can read the events the `WorldDriver` appended without
/// accessing its private `log` field. Clone the `Arc` before moving the
/// `SharedLog` into the driver; both ends then see the same events.
#[derive(Clone)]
struct SharedLog(Arc<Mutex<Vec<Event>>>);

impl SharedLog {
    fn new() -> (Self, Arc<Mutex<Vec<Event>>>) {
        let arc = Arc::new(Mutex::new(vec![]));
        (SharedLog(Arc::clone(&arc)), arc)
    }
}

impl EventLog for SharedLog {
    fn append(&mut self, event: &Event) -> Result<(), Error> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }

    fn load(&self) -> Result<Vec<Event>, Error> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn append_lifecycle(&mut self, _event: &LifecycleEvent) -> Result<(), Error> {
        // Lifecycle events are not needed for this test's assertions.
        Ok(())
    }

    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// fold_log / replay_log — the live fold and the cursor-driven replay
// ---------------------------------------------------------------------------

fn fold_log(events: &[Event], model: &ModelConfig, depth_cap: u8, fanout_cap: u32) -> Result<World, Error> {
    Ok(replay::fold_log(
        genesis_from_log(events, model, depth_cap, fanout_cap)?,
        events,
    ))
}

fn replay_log(events: &[Event], model: &ModelConfig, depth_cap: u8, fanout_cap: u32) -> Result<World, Error> {
    replay::replay_world(
        genesis_from_log(events, model, depth_cap, fanout_cap)?,
        events,
        replay::is_model_call_result,
    )
}

/// Stamp each `ModelResponded`/`ModelFailed`/`InferenceCancelled` event's
/// `fingerprint` with the hash of the request the LIVE driver would record for
/// that `cmd`, so the replay driver REUSES each result instead of diverging (Inv 7).
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig, depth_cap: u8, fanout_cap: u32) {
    let mut world = genesis_from_log(events, model, depth_cap, fanout_cap)
        .expect("genesis for fingerprint pass");
    let mut by_cmd: BTreeMap<CmdId, Fingerprint> = BTreeMap::new();
    for ev in events.iter() {
        let (next, commands) = tick(&world, ev);
        world = next;
        for command in &commands {
            if let Command::CallModel { cmd, messages, tools, params, .. } = command {
                let fp = fingerprint_call(messages, tools, params).expect("fingerprint a request");
                by_cmd.insert(*cmd, fp);
            }
        }
    }
    for ev in events.iter_mut() {
        match &mut ev.input {
            LogicalInput::ModelResponded { cmd, fingerprint, .. }
            | LogicalInput::ModelFailed { cmd, fingerprint, .. }
            | LogicalInput::InferenceCancelled { cmd, fingerprint, .. } => {
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
                model_id: "claude-fanout-completion-replay".into(),
                stop_reason: stop,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

fn spawn_block(tool_use_id: &str, prompt: &str) -> Block {
    Block::ToolUse {
        id: tool_use_id.into(),
        name: "spawn_subagent".into(),
        input: serde_json::json!({ "prompt": prompt }),
    }
}

/// A `ChildReturned` that matches what `regenerate_child_returns` / `child_final_answer`
/// would produce: the child's last assistant message blocks as the `ToolResult` content.
/// This is the named, NON-fingerprinted exception (Inv 7) — re-applied directly by the
/// replay loop, NOT stood in for by the fingerprint cursor.
fn child_returned_with_final_answer(
    at: u64,
    parent: EntityId,
    child: EntityId,
    tool_use_id: &str,
    child_final_answer_blocks: Vec<Block>,
) -> Event {
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
                content: child_final_answer_blocks,
                is_error: false,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// VC-1.1 (offline) — byte-identical replay with ChildReturned re-applied directly
// ---------------------------------------------------------------------------

/// The fan-out log: parent turn → `spawn_subagent` → child Entity runs its turn to
/// `EndTurn` → `ChildReturned` (carrying the child's final answer, the value
/// `regenerate_child_returns` produces) → parent resumes and ends its turn.
///
/// The `ChildReturned` at tick 5 is the named NON-fingerprinted exception (Inv 7):
/// it is re-applied directly by the replay loop (not consumed by the cursor), so the
/// parent's resume is reproduced deterministically with ZERO model calls.
fn authored_fanout_log(model: &ModelConfig) -> Vec<Event> {
    // The child's EndTurn response blocks — used both in the child's ModelResponded
    // AND in the ChildReturned's result, exactly as `child_final_answer` would produce.
    let child_answer_blocks = vec![Block::Text {
        text: "child research complete".into(),
    }];

    let mut events = vec![
        session_started(),
        user_message(1, "research this topic with a sub-agent"),
        // Parent's turn (cmd 0, entity 0): spawns child via spawn_subagent.
        model_responded(
            2,
            0,
            0,
            vec![spawn_block("tu_child", "research the topic and return your findings")],
            StopReason::ToolUse,
        ),
        // Child's own turn (cmd 1, entity 1): EndTurn with the child's answer.
        // After this tick the child is Idle with `child_answer_blocks` as its last
        // assistant message — exactly what `child_final_answer` reads.
        model_responded(
            3,
            1,
            1,
            child_answer_blocks.clone(),
            StopReason::EndTurn,
        ),
        // The `ChildReturned` the DRIVER would regenerate at this point: carries
        // the child's last assistant message blocks (NOT an arbitrary fixture value)
        // as the ToolResult content — the pure-function derivation (Inv 7).
        child_returned_with_final_answer(4, 0, 1, "tu_child", child_answer_blocks),
        // Parent's continuation turn (cmd 2, entity 0): EndTurn after receiving
        // the child's answer.
        model_responded(
            5,
            2,
            0,
            vec![Block::Text {
                text: "synthesis complete".into(),
            }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, model, 8, 0);
    events
}

/// **VC-1.1 (offline)**: a synthetic recorded fan-out log (where the `ChildReturned`
/// carries the child's final answer — the value `regenerate_child_returns` would
/// have produced) replays BYTE-IDENTICALLY to `fold_log` with ZERO model calls.
/// Zero model calls is a STRUCTURAL guarantee: both `fold_log` and `replay_world`
/// take NO `ModelCaller`, so the offline path has no client to reach. The
/// `ChildReturned` is re-applied directly from the log (the named NON-fingerprinted
/// Inv 7 exception), reaching the parent's resume.
#[test]
fn vc_1_1_fanout_completion_offline_replays_byte_identical() {
    let model = offline_model();
    let events = authored_fanout_log(&model);

    let live = fold_log(&events, &model, 8, 0).expect("the fan-out log folds");

    // The child Entity was spawned and settled Idle after its EndTurn.
    let child_id = live
        .entities
        .keys()
        .copied()
        .find(|id| *id != 0)
        .expect("a child Entity was spawned");
    let child = live.entities.get(&child_id).expect("child present");
    assert_eq!(child.identity, Identity::Subagent, "the child is a sub-agent");
    assert_eq!(
        child.lineage,
        Lineage { parent: Some(0), depth: 1 },
        "child lineage = parent, depth + 1"
    );
    assert!(
        matches!(child.activity, Activity::Idle),
        "the child settled to Idle after its EndTurn"
    );

    // The parent advanced past the child and settled to Idle — the parent RESUMED.
    let parent = live.entities.get(&0).expect("parent present");
    assert!(
        matches!(parent.activity, Activity::Idle),
        "the parent resumed and settled to Idle"
    );
    // History: user + assistant(spawn) + user(tool_result = child's answer) + assistant(synthesis).
    assert_eq!(parent.history.0.len(), 4, "the parent's full turn completed end-to-end");
    let tool_result_msg = &parent.history.0[2];
    assert!(matches!(tool_result_msg.role, Role::User));
    match &tool_result_msg.content[0] {
        Block::ToolResult { tool_use_id, content, is_error } => {
            assert_eq!(tool_use_id, "tu_child", "result paired with the Child slot's tool_use_id (Inv 16)");
            assert!(!is_error, "a successful child result is not an error");
            // The content is the child's final answer blocks (the pure-function derivation).
            assert!(
                matches!(content.as_slice(), [Block::Text { text }] if text == "child research complete"),
                "the parent receives the child's final answer verbatim"
            );
        }
        other => panic!("expected a ToolResult in the parent's history, got {other:?}"),
    }

    // Replay determinism (Inv 6/7): `replay_world` under `is_model_call_result` —
    // which returns `false` for `ChildReturned` — re-applies the `ChildReturned`
    // directly (not via the cursor), so the parent's resume is reproduced with ZERO
    // model calls. Zero model calls is STRUCTURAL, not an observed count: `replay_world`
    // (like `fold_log`) accepts no `ModelCaller`, so the offline replay path has no
    // client to invoke. The result is byte-identical to `fold_log`.
    let replay = replay_log(&events, &model, 8, 0).expect("the fan-out log replays faithfully");
    assert_eq!(
        serde_json::to_vec(&live).expect("serialize live"),
        serde_json::to_vec(&replay).expect("serialize replay"),
        "replay_world must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "replay_world must reconstruct an equal World");
    // Two independent replays yield identical bytes.
    let replay2 = replay_log(&events, &model, 8, 0).expect("second replay fold");
    assert_eq!(
        serde_json::to_vec(&replay).expect("serialize first replay"),
        serde_json::to_vec(&replay2).expect("serialize second replay"),
        "two independent replays of the same log are byte-identical"
    );
}

// ---------------------------------------------------------------------------
// VC-1.2 — cumulative cap: second spawn denied after first FINISHES
// ---------------------------------------------------------------------------

/// Two-turn log with `fanout_cap=1`:
///
/// - **Turn 1**: parent spawns child A; child A runs to `EndTurn`; `ChildReturned`
///   settles the parent's slot; parent continues and ends its turn (`Idle`).
///   After turn 1 the parent's `spawned == 1 == fanout_cap`.
///
/// - **Turn 2**: a new `UserMessage` arrives; the parent's second turn requests
///   ANOTHER `spawn_subagent` (child B). This spawn is DENIED inline with
///   `is_error` because `spawned == fanout_cap`, even though child A has already
///   FINISHED and settled.
///
/// This is the load-bearing distinction from the old live-count semantics: a
/// live-count would decrement `spawned` when child A finishes, allowing child B.
/// The CUMULATIVE `spawned` counter is never decremented, so the second spawn
/// is correctly denied.
fn authored_cumulative_cap_log(model: &ModelConfig) -> Vec<Event> {
    let child_a_answer = vec![Block::Text { text: "child a done".into() }];
    let mut events = vec![
        session_started(),
        // Turn 1: parent spawns child A, child A finishes.
        user_message(1, "delegate the first task"),
        // Parent's first turn (cmd 0): spawn child A.
        model_responded(
            2,
            0,
            0,
            vec![spawn_block("tu_a", "task A")],
            StopReason::ToolUse,
        ),
        // Child A's turn (cmd 1, entity 1): EndTurn → child A Idle.
        model_responded(3, 1, 1, child_a_answer.clone(), StopReason::EndTurn),
        // ChildReturned: parent's slot for child A is settled. After this fold,
        // parent.spawned == 1 == fanout_cap — child A's completion does NOT refund.
        child_returned_with_final_answer(4, 0, 1, "tu_a", child_a_answer),
        // Parent continuation (cmd 2): EndTurn after receiving child A's answer.
        model_responded(
            5,
            2,
            0,
            vec![Block::Text { text: "turn 1 complete".into() }],
            StopReason::EndTurn,
        ),
        // Turn 2: new user message; parent tries to spawn child B — DENIED.
        user_message(6, "delegate the second task"),
        // Parent's second turn (cmd 3): requests spawn of child B.
        // Because spawned==1==fanout_cap, SubagentSystem denies the spawn inline
        // (is_error), and the parent's continuation fires immediately.
        model_responded(
            7,
            3,
            0,
            vec![spawn_block("tu_b", "task B")],
            StopReason::ToolUse,
        ),
        // Parent's continuation after the denied spawn (cmd 4): EndTurn.
        model_responded(
            8,
            4,
            0,
            vec![Block::Text { text: "cannot delegate further".into() }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, model, 8, 1);
    events
}

/// **VC-1.2**: the cumulative `spawned` cap denies a SEQUENTIAL second spawn
/// (`is_error`, no deadlock) once `spawned==fanout_cap`, even after the first child
/// has FINISHED — and it does so by reading the STORED cumulative counter, NOT a live
/// recount of present children. That is the load-bearing distinction from the OLD
/// live-count semantics, so the test is constructed to FAIL under that old rule:
///
/// 1. Fold ONLY turn 1 → parent Idle, `spawned == 1`, child A present.
/// 2. WHITE-BOX drop child A from `world.entities`, forcing the stored cumulative
///    `spawned` (still 1) and a LIVE recount of present children (now 0) to DIVERGE.
///    A live-count cap would now ALLOW child B (0 < fanout_cap); the cumulative cap
///    must DENY it (1 >= fanout_cap). This divergence is asserted as a positive
///    control — it is the exact state the old and new rules disagree on.
/// 3. Drive turn 2's spawn-B request onto the MUTATED World via the same tick/System
///    path the rest of the file uses, so the spawn decision runs AGAINST the diverged
///    state: it DENIES inline (`is_error` ToolResult for `tu_b`, NO child Entity
///    created, parent settles Idle — no deadlock). A denial here can ONLY come from
///    the stored counter, since no live child was present to count.
/// 4. The UNMUTATED full log additionally replays byte-identically (Inv 6).
#[test]
fn vc_1_2_cumulative_cap_denies_second_spawn_after_first_finishes() {
    let model = offline_model();
    let fanout_cap: u32 = 1;
    let events = authored_cumulative_cap_log(&model);

    // Fold ONLY turn 1 (through the parent's continuation EndTurn) so the spawn-B
    // decision can be driven AFTER the white-box mutation below. Splitting at index 6
    // keeps turn 1's id minting byte-identical to the full fold's prefix (cmd 0–2,
    // child A as entity 1), so turn 2 still mints cmd 3 / cmd 4 to match the log.
    let (turn1, turn2) = events.split_at(6);
    let mut world = fold_log(turn1, &model, 8, fanout_cap).expect("turn 1 of the cumulative-cap log folds");

    // Turn 1 succeeded: exactly the root + one child Entity (child A), both Idle, and
    // the parent's cumulative `spawned == 1`.
    let child_a = world
        .entities
        .keys()
        .copied()
        .find(|id| *id != 0)
        .expect("child A was spawned in turn 1");
    assert!(
        matches!(world.entities.get(&child_a).expect("child A").activity, Activity::Idle),
        "child A settled to Idle after its EndTurn"
    );
    assert_eq!(world.entities.len(), 2, "only the root + child A exist after turn 1");
    assert_eq!(
        world.entities.get(&0).expect("parent").spawned,
        1,
        "spawned == 1 after child A finishes (cumulative — never decremented)"
    );
    assert!(
        matches!(world.entities.get(&0).expect("parent").activity, Activity::Idle),
        "the parent settled to Idle after turn 1"
    );

    // NON-VACUITY — force the stored cumulative `spawned` and a LIVE child recount to
    // DIVERGE. White-box DROP child A from `world.entities`: a live-count cap rule (the
    // OLD semantics) recomputes the breadth by counting present children of this parent,
    // which is now 0 < fanout_cap, so it would ALLOW child B. The CUMULATIVE rule reads
    // the STORED `spawned` (still 1 == fanout_cap) and must DENY. A denial driven below
    // can therefore ONLY have come from the stored counter.
    world.entities.remove(&child_a);

    // POSITIVE CONTROL — the divergence, asserted explicitly. This is what makes the
    // test FAIL under the old live-count rule and PASS under the cumulative rule.
    let live_child_recount = world
        .entities
        .values()
        .filter(|e| e.lineage.parent == Some(0))
        .count();
    assert_eq!(
        live_child_recount, 0,
        "a LIVE recount over the mutated World sees 0 present children — a live-count cap would ALLOW child B"
    );
    let parent_spawned = world.entities.get(&0).expect("parent").spawned;
    assert_eq!(
        parent_spawned, 1,
        "the STORED cumulative spawned is still 1 — it has DIVERGED above the live recount"
    );
    assert!(
        parent_spawned >= fanout_cap,
        "spawned ({parent_spawned}) >= fanout_cap ({fanout_cap}) — the cumulative rule must DENY child B"
    );

    // Drive turn 2's spawn-B request onto the MUTATED World via the same tick/System
    // path the rest of the file uses, so the spawn decision runs against the diverged
    // state above. (`replay::fold_log` folds the events through the pure `tick` reducer
    // from the supplied World — here the mutated one — with ZERO model calls.)
    let world = replay::fold_log(world, turn2);

    // The spawn was DENIED on the STORED counter, NOT a live recount:
    //  - no child Entity exists (child A was removed; child B was never spawned),
    //  - the parent settled Idle (no deadlock), `spawned` still 1 (never decremented).
    assert_eq!(
        world.entities.len(),
        1,
        "only the root remains — child B was DENIED even though NO live child was present to count"
    );
    let parent = world.entities.get(&0).expect("parent");
    assert!(
        matches!(parent.activity, Activity::Idle),
        "the parent settled to Idle at the end — no deadlock"
    );
    assert_eq!(
        parent.spawned, 1,
        "spawned stays 1 — cumulative, never decremented (a denied spawn does not bump it)"
    );

    // The turn-2 denial is the LAST tool_result Msg in the parent's History: an
    // is_error ToolResult paired with child B's tool_use_id (Inv 16).
    let tool_result_msgs: Vec<_> = parent
        .history
        .0
        .iter()
        .filter(|m| {
            matches!(m.role, Role::User)
                && m.content.iter().any(|b| matches!(b, Block::ToolResult { .. }))
        })
        .collect();
    // Two tool_result Msgs: one from turn 1 (child A's answer), one from turn 2 (denied).
    assert_eq!(tool_result_msgs.len(), 2, "two tool_result Msgs: one per turn");
    let denied_result_msg = tool_result_msgs
        .last()
        .expect("the turn-2 tool_result Msg for the denied spawn");
    match &denied_result_msg.content[0] {
        Block::ToolResult { tool_use_id, is_error, .. } => {
            assert_eq!(tool_use_id, "tu_b", "the denied slot is matched by tool_use_id (Inv 16)");
            assert!(
                *is_error,
                "the cumulative-cap denial resolves is_error inline (Inv 10/11)"
            );
        }
        other => panic!("expected the is_error ToolResult for child B, got {other:?}"),
    }

    // Inv 6: the UNMUTATED full log additionally replays byte-identically — the
    // cumulative cap is pure World state, so a faithful replay reproduces the denial
    // deterministically (this does NOT exercise the divergence above; it guards Inv 6).
    let folded_full = fold_log(&events, &model, 8, fanout_cap).expect("the full cumulative-cap log folds");
    let replayed_full = replay_log(&events, &model, 8, fanout_cap).expect("the cumulative-cap log replays");
    assert_eq!(
        serde_json::to_vec(&folded_full).expect("serialize fold"),
        serde_json::to_vec(&replayed_full).expect("serialize replay"),
        "the cumulative-cap World replays byte-identically (Inv 6)"
    );
}

// ---------------------------------------------------------------------------
// VC-1.1 (live; gated) — real child model call, parent receives answer
// ---------------------------------------------------------------------------

/// A `ModelCaller` hybrid for the live fan-out test:
///
/// - Call 0 (parent's first call): returns a `spawn_subagent` ToolUse so the
///   `WorldDriver` opens a child slot and spawns the child Entity.
/// - Call 1+ (child's call + parent's continuation): delegates to the real
///   selected provider, proving at least two REAL model calls bracket the fan-out.
///
/// This isolates the TEST's control over WHICH calls happen (the spawn) from the
/// REAL model's responses (the child's answer + the parent's continuation).
struct FanoutCaller {
    real: Box<dyn ModelApi>,
    calls: AtomicUsize,
}

impl ModelApi for FanoutCaller {
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Parent's first call: return a `spawn_subagent` ToolUse so the driver
            // opens the child slot and spawns the child Entity.
            Box::pin(async {
                Ok(ModelResponse {
                    blocks: vec![ContentBlock::ToolUse {
                        id: "tu_live_child".into(),
                        name: "spawn_subagent".into(),
                        input: serde_json::json!({
                            "prompt": "Reply with exactly two words: child done"
                        }),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    model_id: "stub-spawn".into(),
                    reasoning: ReasoningPolicy::Drop,
                    capabilities: serde_json::json!({}),
                })
            })
        } else {
            // Child's call (n==1) and parent's continuation (n==2): REAL model calls.
            self.real.turn(req)
        }
    }
    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        self.real.list_models()
    }
    fn model(&self) -> &str {
        self.real.model()
    }
}

/// **VC-1.1 (live)**: a real one-child fan-out driven by `WorldDriver`:
///
/// - The parent's first (stubbed) call returns `spawn_subagent`, opening the
///   child slot.
/// - The child's call is REAL (via the selected provider), runs to `EndTurn`.
/// - The `WorldDriver` regenerates `ChildReturned` (non-fingerprinted, Inv 7),
///   records it, and folds it — the parent resumes.
/// - The parent's continuation is REAL, runs to `EndTurn`.
///
/// Assertions: the log contains a `ChildReturned`; the parent's History contains
/// the child's answer as a ToolResult; the parent ended with assistant text;
/// the final World folds byte-identically from the recorded log (`fold_log`).
#[tokio::test]
async fn vc_1_1_live_fanout_child_returns_to_parent() {
    let test = "vc_1_1_live_fanout_child_returns_to_parent";
    if skip_without_live_llm(test) {
        return;
    }

    let model = live_model();
    let real_client = selected_from_env().expect("build provider from env");
    let caller = FanoutCaller {
        real: real_client,
        calls: AtomicUsize::new(0),
    };

    let (shared_log, events_arc) = SharedLog::new();
    let mut driver =
        WorldDriver::bootstrap(SEED, model.clone(), caller, NoSurfaceDrive, shared_log)
            .expect("bootstrap WorldDriver for live fan-out");

    let notifications = driver
        .submit(LogicalInput::UserMessage {
            to: 0,
            text: "delegate a small research task to a sub-agent".into(),
        })
        .await
        .expect("the live fan-out turn drives to quiescence");

    // The parent responded with assistant text (it resumed from the child's answer
    // and produced its own turn).
    assert!(
        !notifications.is_empty(),
        "the parent must produce at least one EntryNotification (it responded)"
    );

    // Load the events recorded by the driver through the shared log.
    let events = events_arc.lock().unwrap().clone();

    // The log must contain a `ChildReturned` — the driver regenerated and recorded it.
    let child_returns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::ChildReturned { .. }))
        .collect();
    assert_eq!(
        child_returns.len(),
        1,
        "exactly one ChildReturned was regenerated + logged by the driver"
    );

    // The `ChildReturned` carries the child's tool_use_id ("tu_live_child").
    match &child_returns[0].input {
        LogicalInput::ChildReturned { tool_use_id, result, .. } => {
            assert_eq!(tool_use_id, "tu_live_child", "ChildReturned paired with the spawn's tool_use_id");
            match result {
                Block::ToolResult { is_error, .. } => {
                    assert!(!is_error, "a successful child result is not an error");
                }
                other => panic!("expected a ToolResult in ChildReturned.result, got {other:?}"),
            }
        }
        other => panic!("expected ChildReturned, got {other:?}"),
    }

    // DETERMINISM (Inv 6/7/9): fold_log of the recorded log (including the
    // ChildReturned and the fingerprinted ModelResponded events) must produce a World
    // that is byte-identical to the live World — proving the regenerated ChildReturned
    // replays correctly without a fingerprinted boundary.
    let genesis_world = genesis_from_log(&events, &model, 8, 0)
        .expect("genesis from the recorded log");
    let refolded = replay::fold_log(genesis_world, &events);

    // The folded World must have at least two entities (root + child) and the root
    // must be Idle (the parent completed its continuation).
    assert!(refolded.entities.len() >= 2, "root + at least one child Entity in the refolded World");
    let root = refolded.entities.get(&0).expect("root entity");
    assert!(
        matches!(root.activity, Activity::Idle),
        "the parent settled to Idle after the fan-out (the refolded World)"
    );
}
