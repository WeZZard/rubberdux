//! VC-2.3 / VC-2.4 — the SAFETY integration sink for BOUNDEDNESS and
//! REPLAY-WITH-WAL-PRESENT, composed over the same `src/agent/world/` stack the
//! walking skeleton wires (genesis, the pure `tick` reducer, the LIVE driver
//! `drive_live`, and the REPLAY driver `drive_replay` + `ReplayCursor`). It touches
//! no `src/agent/world/*` file.
//!
//! - **VC-2.3 (boundedness brakes, Inv 11)** — three explicit, replay-deterministic
//!   brakes, each driven through the full `tick` reducer:
//!   - the per-entity `loop_cap` forces ONE final wrap-up turn (a wrap-up directive is
//!     injected) and then STOPS initiating further turns;
//!   - an Inbox overflow drops the OLDEST queued message (DropOldest backpressure, a
//!     stratum-1 World change) and the LIVE driver appends a stratum-2
//!     `MessageDropped { InboxOverflow }` notice (asserted via `load_lifecycle`);
//!   - a `fanout_cap`-exceeding spawn resolves the slot `is_error` WITHOUT growing
//!     `entities` (Inv 10 — a capped spawn never leaves the parent waiting).
//! - **VC-2.4 (replay with WAL present, Inv 6/9)** — a log that INCLUDES stratum-2
//!   records (`CommandDispatched`, `MessageDropped`) replays to a BYTE-IDENTICAL
//!   `World` versus the live fold, with ZERO model/tool re-invocation (an exploding
//!   client) — proving stratum 2 is NEUTRAL on replay (the replay fold consumes
//!   stratum 1 alone). Includes the SECOND independent replay byte-identity assertion.
//!
//! Every log is hand-authored synthetic; none makes a live model call, so the file
//! needs no credentials and runs on every developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), 9 (log = single
//! source of truth), 10 (totality — no stuck state), 11 (boundedness brakes).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::budget::{Budget, Limits};
use rubberdux::agent::world::effects::{
    Command, ModelCaller, ReplayCursor, Replayed, ResultStamp, SurfaceDriver, drive_live,
    drive_replay, fingerprint_call,
};
use rubberdux::agent::world::surface::SurfaceView;
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Role};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::lifecycle::{DropReason, LifecycleEvent};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, SlotKind,
    SlotState, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8). Inert here — no System
// draws — but it must reseed identically for the live fold and the replay to match.
const SEED: u64 = 7;

/// The loop-cap wrap-up directive IntakeSystem injects on the FINAL turn once the loop
/// cap is reached (see `src/agent/world/systems/intake.rs` — `LOOP_CAP_WRAP_UP`). Kept
/// in sync by value here so the integration test can detect the forced wrap-up turn
/// without reaching into the private const.
const LOOP_CAP_WRAP_UP: &str = "step budget exhausted; finalize now";

// ---------------------------------------------------------------------------
// Genesis / fold / replay / fingerprint-stamp harness (parameterized by Limits)
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary entity at `Idle`
/// whose per-entity `Budget.limits` carry the brake under test, and `Resources` seeded
/// from the session's `seed`. The shell owns genesis, and BOTH the live fold and the
/// replay reconstruct it the SAME way — which is what makes the two Worlds comparable
/// byte-for-byte. Mirrors the walking-skeleton genesis with the brakes threaded in.
fn genesis(seed: u64, model: &ModelConfig, limits: Limits) -> World {
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
            budget: Budget {
                limits,
                ..Default::default()
            },
            inbox: Inbox::default(),
            turns: 0,
            model: None,
        },
    );
    world
}

/// Rebuild genesis from the recorded log: the seed is the one hidden input that must
/// cross a recorded boundary, so replay reseeds `Rng` from the log's `SessionStarted`
/// header rather than from any live source. The `limits` are genesis-parity (the same
/// value the live run used), like the world-default `ModelConfig`.
fn genesis_from_log(events: &[Event], model: &ModelConfig, limits: Limits) -> Result<World, Error> {
    let seed = events
        .iter()
        .find_map(|e| match &e.input {
            LogicalInput::SessionStarted { seed, .. } => Some(*seed),
            _ => None,
        })
        .ok_or_else(|| Error::World("recorded log has no SessionStarted header".into()))?;
    Ok(genesis(seed, model, limits))
}

fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-bounds-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// Model-call stand-ins
// ---------------------------------------------------------------------------

/// A `ModelCaller` that records any invocation and then panics. `drive_replay` takes NO
/// client, so this can never be threaded into `replay_log` by construction; asserting
/// its counter stays zero makes the "zero model/tool calls" guarantee (Inv 6) explicit,
/// while the panic is the structural backstop.
struct ExplodingClient {
    calls: Arc<AtomicUsize>,
}

impl ModelCaller for ExplodingClient {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("the replay driver must never invoke the model client");
    }
}

/// A `ModelCaller` returning a fixed successful response, so `drive_live` produces a
/// `ModelResponded` without a network — used only to HARVEST the stratum-2 records the
/// live driver writes (the stratum-1 results it returns are discarded). Mirrors the
/// effects.rs unit-test stub.
struct StubClient;

impl ModelCaller for StubClient {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        Ok((
            vec![Block::Text { text: "ok".into() }],
            ModelMeta {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                model_id: "claude-bounds-replay".into(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        ))
    }
}

/// A no-op surface-drive sink: these bounds tests drive only `CallModel`/
/// `EmitLifecycle` Commands (no `set_value` RunTool), so the driver is never
/// reached — it stands in for the injected sink so `drive_live` type-checks.
struct NoSurfaceDrive;
impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// fold_log / replay_log / stamp_fingerprints — the shared determinism harness
// ---------------------------------------------------------------------------

/// The canonical ("live") World: re-apply every recorded Event through `tick`.
fn fold_log(events: &[Event], model: &ModelConfig, limits: Limits) -> Result<World, Error> {
    let mut world = genesis_from_log(events, model, limits)?;
    for ev in events {
        let (next, _commands) = tick(&world, ev);
        world = next;
    }
    Ok(world)
}

/// Whether an input is a model-call RESULT the replay driver stands in for via the
/// `ReplayCursor` (the `CallModel` duals). Every OTHER recorded input — the exogenous
/// free variables AND the `ToolReturned` tool results — is re-applied directly by the
/// loop, exactly as crash-resume re-applies a recorded Input through the reducer.
fn is_model_call_result(input: &LogicalInput) -> bool {
    matches!(
        input,
        LogicalInput::ModelResponded { .. }
            | LogicalInput::ModelFailed { .. }
            | LogicalInput::InferenceCancelled { .. }
    )
}

/// Fold the SAME recorded log from genesis under the REPLAY driver. The loop re-applies
/// every input EXCEPT the model-call results, and for each tick's Commands it calls
/// `drive_replay` — which takes NO `ModelCaller` and so cannot call the model by
/// construction — standing in the already-logged result while the re-emitted request
/// re-hashes to its `Fingerprint`. The `ReplayCursor` is built over the model-call
/// results ALONE, so a continuation `CallModel` re-emitted after a `ToolReturned` pulls
/// the NEXT `ModelResponded` from the cursor. A `Diverged` outcome is a replay failure.
/// Mirrors the walking-skeleton / tool-loop replay harness.
fn replay_log(events: &[Event], model: &ModelConfig, limits: Limits) -> Result<World, Error> {
    let mut world = genesis_from_log(events, model, limits)?;
    let model_results: Vec<Event> = events
        .iter()
        .filter(|e| is_model_call_result(&e.input))
        .cloned()
        .collect();
    let mut cursor = ReplayCursor::new(&model_results);

    for ev in events.iter().filter(|e| !is_model_call_result(&e.input)) {
        let (next, mut commands) = tick(&world, ev);
        world = next;

        while !commands.is_empty() {
            let replayed = drive_replay(&commands, &mut cursor)?;
            commands = Vec::new();
            for outcome in replayed {
                match outcome {
                    Replayed::Reused(event) => {
                        let (next, mut cmds) = tick(&world, &event);
                        world = next;
                        commands.append(&mut cmds);
                    }
                    Replayed::Diverged => {
                        return Err(Error::World(
                            "replay diverged: a re-emitted request did not match the \
                             recorded result, but a faithful replay must reuse every result"
                                .into(),
                        ));
                    }
                }
            }
        }
    }

    Ok(world)
}

/// Stamp each model-call result's `fingerprint` with the value the LIVE driver would
/// have recorded (the hash of the request the reducer re-emits for that `cmd`), so the
/// replay reuses each result instead of diverging (Inv 7) — without hand-authoring any
/// continuation `History`.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig, limits: Limits) {
    let mut world = genesis_from_log(events, model, limits).expect("genesis for fingerprints");
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

/// Harvest exactly the stratum-2 records the LIVE driver writes while folding `events`:
/// the `CommandDispatched` dispatch-intent per `CallModel` and the `MessageDropped`
/// notice carried by each `EmitLifecycle` (an Inbox overflow). It dry-folds the log and,
/// for each tick's emitted Commands, runs `drive_live` against a THROWAWAY log with a
/// stub client, keeping only `load_lifecycle()` and discarding the throwaway stratum-1
/// results. These genuine records are appended to the replay log to prove stratum 2 is
/// present-yet-neutral. (`RunTool` emits no dispatch-intent in this milestone.)
async fn collect_stratum2(
    events: &[Event],
    model: &ModelConfig,
    limits: Limits,
) -> Vec<LifecycleEvent> {
    let mut world = genesis_from_log(events, model, limits).expect("genesis for stratum-2 harvest");
    let mut harvested = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        let (next, commands) = tick(&world, ev);
        world = next;
        if commands.is_empty() {
            continue;
        }
        let mut throwaway = MemoryEventLog::new();
        let stamp = ResultStamp {
            edge: ev.edge,
            app_edge: ev.edge,
            at: 1_000 + i as u64,
            wall: None,
        };
        drive_live(
            &commands,
            stamp,
            &SurfaceView::new(),
            &StubClient,
            &NoSurfaceDrive,
            &mut throwaway,
        )
        .await
        .expect("drive_live for the stratum-2 harvest");
        harvested.extend(throwaway.load_lifecycle().expect("load harvested stratum-2"));
    }
    harvested
}

// ---------------------------------------------------------------------------
// Event builders — keep the hand-authored logs readable
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

/// A `ModelResponded` for the primary entity. Fingerprint is a placeholder filled by
/// `stamp_fingerprints` (for replayed logs); the loop-cap / fan-out tick-only tests do
/// not replay, so its placeholder is inert there.
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
                model_id: "claude-bounds-replay".into(),
                stop_reason: stop,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

fn tool_returned(at: u64, cmd: CmdId, text: &str) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ToolReturned {
            cmd,
            entity: 0,
            fingerprint: Fingerprint("fp-tool-result".into()),
            result: vec![Block::Text { text: text.into() }],
        },
    }
}

/// A `ToolUse` block resolved by a `Local` tool (`RunTool`) — any name that is not a
/// `spawn_subagent`/`ask_human` routes to a Local slot.
fn local_tool_use(id: &str, name: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

/// A `ModelResponded·ToolUse` asking for TWO sub-agents at once — two `Child` slots in
/// one assistant turn, so SubagentSystem decides both in one batch (mirrors the
/// subagent unit fixture). Used only by the fan-out cap tick test (not replayed).
fn spawn_two_subagents(cmd: CmdId, tu_a: &str, tu_b: &str, at: u64) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            fingerprint: Fingerprint("fp".into()),
            blocks: vec![
                Block::ToolUse {
                    id: tu_a.into(),
                    name: "spawn_subagent".into(),
                    input: serde_json::json!({ "prompt": "a" }),
                },
                Block::ToolUse {
                    id: tu_b.into(),
                    name: "spawn_subagent".into(),
                    input: serde_json::json!({ "prompt": "b" }),
                },
            ],
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-bounds-replay".into(),
                stop_reason: StopReason::ToolUse,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

/// The `cmd` of the first `CallModel` in a tick's emitted Commands.
fn call_model_cmd(commands: &[Command]) -> CmdId {
    commands
        .iter()
        .find_map(|c| match c {
            Command::CallModel { cmd, .. } => Some(*cmd),
            _ => None,
        })
        .expect("expected a CallModel command")
}

/// Whether a tick's `CallModel` request History carries the loop-cap wrap-up directive
/// (the forced final-turn marker IntakeSystem injects).
fn carries_wrap_up(commands: &[Command]) -> bool {
    commands.iter().any(|c| match c {
        Command::CallModel { messages, .. } => messages.0.iter().any(|m| {
            matches!(m.role, Role::User)
                && m.content
                    .iter()
                    .any(|b| matches!(b, Block::Text { text } if text == LOOP_CAP_WRAP_UP))
        }),
        _ => false,
    })
}

fn activity_of(world: &World) -> &Activity {
    &world.entities.get(&0).expect("primary entity present").activity
}

// ---------------------------------------------------------------------------
// VC-2.3 (a) — loop_cap forces a final wrap-up turn, then STOPS (Inv 11)
// ---------------------------------------------------------------------------

/// **VC-2.3 (loop guard)** With `loop_cap = 2`, the first turn is NORMAL, the second is
/// the FORCED FINAL wrap-up turn (a wrap-up directive is injected so the model
/// concludes), and a third `UserMessage` initiates NO further turn — the loop is braked.
/// The per-entity turn counter is World state, so the brake replays deterministically.
#[test]
fn vc_2_3_loop_cap_forces_final_wrap_up_then_stops() {
    let model = offline_model();
    let world = genesis(
        SEED,
        &model,
        Limits {
            loop_cap: 2,
            ..Default::default()
        },
    );

    // Turn 1 (turns 0 → 1): a NORMAL turn — no wrap-up directive.
    let (world, c1) = tick(&world, &user_message(1, "u1"));
    assert!(
        !carries_wrap_up(&c1),
        "a sub-cap turn carries no wrap-up directive"
    );
    let cmd1 = call_model_cmd(&c1);
    let (world, _) = tick(
        &world,
        &model_responded(2, cmd1, vec![Block::Text { text: "ok".into() }], StopReason::EndTurn),
    );
    assert!(matches!(activity_of(&world), Activity::Idle), "turn 1 settles to Idle");

    // Turn 2 (turns 1 → 2 == cap): the FINAL wrap-up turn — directive injected.
    let (world, c2) = tick(&world, &user_message(3, "u2"));
    assert!(
        carries_wrap_up(&c2),
        "the cap-reaching turn injects the wrap-up directive (forces the model to finalize)"
    );
    let cmd2 = call_model_cmd(&c2);
    let (world, _) = tick(
        &world,
        &model_responded(4, cmd2, vec![Block::Text { text: "ok".into() }], StopReason::EndTurn),
    );
    assert!(matches!(activity_of(&world), Activity::Idle), "turn 2 settles to Idle");

    // Turn 3 (turns 2 >= cap): STOP — no further turn is initiated.
    let history_before = world.entities.get(&0).expect("entity").history.0.len();
    let (world, c3) = tick(&world, &user_message(5, "u3"));
    assert!(
        !c3.iter().any(|c| matches!(c, Command::CallModel { .. })),
        "past the loop cap no further turn is initiated (stop)"
    );
    let e = world.entities.get(&0).expect("entity");
    assert!(matches!(e.activity, Activity::Idle), "the looped-out entity stays Idle");
    assert_eq!(
        e.history.0.len(),
        history_before,
        "a stopped initiation consumes no message into History"
    );
}

// ---------------------------------------------------------------------------
// VC-2.3 (b) — Inbox overflow drops oldest; LIVE driver appends MessageDropped
// ---------------------------------------------------------------------------

/// **VC-2.3 (Inbox DropOldest)** A mid-run `UserMessage` steered onto a full, capped
/// Inbox evicts the OLDEST entry (the eviction is a stratum-1 World change, so replay
/// reconstructs the bounded Inbox), and the LIVE driver appends the stratum-2
/// `MessageDropped { InboxOverflow }` notice to the log via `append_lifecycle` (asserted
/// via `load_lifecycle`). A steering overflow emits NO model call, so the client is
/// never reached (Inv 11).
#[tokio::test]
async fn vc_2_3_inbox_overflow_drops_oldest_and_live_driver_logs_message_dropped() {
    let model = offline_model();

    // Drive a real turn so the entity is genuinely mid-run (`Thinking`), then cap and
    // pre-fill its Inbox so the next steer overflows.
    let world = genesis(SEED, &model, Limits::default());
    let (mut world, _c) = tick(&world, &user_message(1, "u1"));
    assert!(
        matches!(activity_of(&world), Activity::Thinking { .. }),
        "the entity is mid-run after initiating its turn"
    );
    {
        let e = world.entities.get_mut(&0).expect("entity");
        e.budget.limits.inbox_capacity = 1;
        e.inbox.pending = vec![vec![Block::Text { text: "old".into() }]];
    }

    // The mid-run steer overflows the cap → tick evicts "old" (stratum-1) and emits the
    // stratum-2 notice Command; it starts NO turn.
    let (world, commands) = tick(&world, &user_message(2, "steer"));
    assert!(
        !commands.iter().any(|c| matches!(c, Command::CallModel { .. })),
        "a mid-run steer starts no turn"
    );
    assert!(!commands.is_empty(), "an overflowing steer emits the stratum-2 notice Command");
    assert_eq!(
        world.entities.get(&0).expect("entity").inbox.pending,
        vec![vec![Block::Text { text: "steer".into() }]],
        "the eviction is in the stratum-1 fold: oldest dropped, FIFO preserved, bounded at the cap"
    );

    // The LIVE driver appends the MessageDropped notice to stratum 2; it makes no model
    // call (the steering overflow has no effectful Command), so the client is unreached.
    let mut log = MemoryEventLog::new();
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let stamp = ResultStamp {
        edge: 0,
        app_edge: 1,
        at: 3,
        wall: None,
    };
    let results = drive_live(
        &commands,
        stamp,
        &SurfaceView::new(),
        &exploding,
        &NoSurfaceDrive,
        &mut log,
    )
    .await
    .expect("the live driver appends the notice");
    assert!(
        results.is_empty(),
        "the notice produces no stratum-1 result Event (it is stratum-2 only)"
    );
    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "a steering overflow invokes the model client zero times"
    );

    let lifecycle = log.load_lifecycle().expect("load stratum-2");
    assert!(
        lifecycle.iter().any(|ev| matches!(
            ev,
            LifecycleEvent::MessageDropped { reason: DropReason::InboxOverflow, dropped, .. }
                if *dropped >= 1
        )),
        "the Inbox overflow notice reaches the stratum-2 log via append_lifecycle (Inv 11)"
    );
}

// ---------------------------------------------------------------------------
// VC-2.3 (c) — fanout_cap denies a spawn is_error without growing entities
// ---------------------------------------------------------------------------

/// **VC-2.3 (fan-out cap, Inv 10/11)** With `fanout_cap = 1`, a turn requesting TWO
/// sub-agents spawns the FIRST and DENIES the second: the denied `Child` slot resolves
/// `is_error` WITHOUT inserting a child Entity, so `entities` does not grow past parent
/// + one child — a capped spawn never leaves the parent waiting on a child that will
/// never exist.
#[test]
fn vc_2_3_fanout_cap_denies_spawn_is_error_without_growing_entities() {
    let model = offline_model();
    let world = genesis(
        SEED,
        &model,
        Limits {
            fanout_cap: 1,
            ..Default::default()
        },
    );

    let (world, c) = tick(&world, &user_message(1, "go"));
    let turn_cmd = call_model_cmd(&c);
    let (world, commands) = tick(&world, &spawn_two_subagents(turn_cmd, "tu_a", "tu_b", 2));

    // Only ONE child Entity exists (parent + one child) — the 2nd spawn is denied.
    assert_eq!(
        world.entities.len(),
        2,
        "the fan-out cap denies the 2nd spawn — no 2nd child Entity is inserted"
    );
    // Exactly one child CallModel was dispatched (the admitted child's turn).
    assert_eq!(
        commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { entity, .. } if *entity != 0))
            .count(),
        1,
        "only the admitted child's turn is dispatched"
    );

    let slots = match activity_of(&world) {
        Activity::ResolvingToolUses { slots } => slots.clone(),
        other => panic!("expected ResolvingToolUses, got {other:?}"),
    };
    assert_eq!(slots.len(), 2, "both tool_use blocks became slots");

    // tu_a: admitted — the child exists and the slot is still Pending (awaiting return).
    let slot_a = slots.iter().find(|s| s.tool_use_id == "tu_a").expect("slot a");
    assert_eq!(
        slot_a.state,
        SlotState::Pending { cmd: None },
        "the admitted spawn stays Pending (Inv 16)"
    );
    let child_a = match slot_a.kind {
        SlotKind::Child(c) => c,
        other => panic!("expected a Child slot, got {other:?}"),
    };
    assert!(world.entities.contains_key(&child_a), "the admitted child exists in the World");

    // tu_b: denied — settled `is_error`, NO child Entity created.
    let slot_b = slots.iter().find(|s| s.tool_use_id == "tu_b").expect("slot b");
    assert_eq!(slot_b.state, SlotState::Done, "the denied spawn is settled, not left pending");
    let child_b = match slot_b.kind {
        SlotKind::Child(c) => c,
        other => panic!("expected a Child slot, got {other:?}"),
    };
    assert!(!world.entities.contains_key(&child_b), "the denied spawn created NO child Entity");
    match &slot_b.result {
        Some(Block::ToolResult { is_error, tool_use_id, .. }) => {
            assert!(*is_error, "a fan-out-capped spawn resolves is_error (Inv 10)");
            assert_eq!(tool_use_id, "tu_b");
        }
        other => panic!("expected an is_error ToolResult, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// VC-2.4 — a log WITH stratum-2 records replays byte-identical (stratum-2 neutral)
// ---------------------------------------------------------------------------

/// Build the synthetic bounded-session log: a tool turn that holds the entity mid-run
/// (`ResolvingToolUses`) while two steers arrive on a capped (cap 1) Inbox — the second
/// overflows, dropping the oldest — then the tool result completes the turn and a final
/// `EndTurn` settles the entity, leaving the surviving steer parked. The fingerprints
/// are stamped to the exact requests the reducer re-emits, so the replay reuses each
/// model result (Inv 7).
fn authored_bounded_log(model: &ModelConfig, limits: Limits) -> Vec<Event> {
    let mut events = vec![
        session_started(),
        user_message(1, "u1"),
        // The tool turn (cmd 0): branches into ONE Local slot, RunTool cmd 1 in flight —
        // the entity stays in ResolvingToolUses (mid-run) awaiting the result.
        model_responded(2, 0, vec![local_tool_use("tu_x", "tool_x")], StopReason::ToolUse),
        // Two mid-run steers on the cap-1 Inbox: the FIRST parks; the SECOND overflows,
        // dropping the oldest (a stratum-1 eviction + a stratum-2 MessageDropped notice).
        user_message(3, "steer1"),
        user_message(4, "steer2"),
        // The tool result completes the turn → continuation CallModel cmd 2.
        tool_returned(5, 1, "result X"),
        // The continuation answers cmd 2 with EndTurn → Idle, leaving "steer2" parked.
        model_responded(6, 2, vec![Block::Text { text: "done".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut events, model, limits);
    events
}

/// **VC-2.4** A log that INCLUDES stratum-2 records (`CommandDispatched`,
/// `MessageDropped`) replays to a BYTE-IDENTICAL `World` versus the live fold, with ZERO
/// model/tool re-invocation — proving stratum 2 is NEUTRAL on replay: the replay fold
/// consumes stratum 1 alone (`load()`), never the operational stream (`load_lifecycle`).
/// Includes the SECOND independent replay byte-identity assertion (Inv 6, 9).
#[tokio::test]
async fn vc_2_4_log_with_wal_present_replays_byte_identical_stratum_2_neutral() {
    let model = offline_model();
    // The cap that makes the second steer overflow — genesis-parity across fold/replay.
    let limits = Limits {
        inbox_capacity: 1,
        ..Default::default()
    };
    let events = authored_bounded_log(&model, limits);

    // --- The canonical ("live") World, and its bounded shape ------------------
    let live = fold_log(&events, &model, limits).expect("the bounded log folds");
    let e = live.entities.get(&0).expect("primary entity present");
    assert!(matches!(e.activity, Activity::Idle), "the tool loop settles to Idle");
    assert_eq!(
        e.inbox.pending,
        vec![vec![Block::Text { text: "steer2".into() }]],
        "the Inbox is bounded at the cap: 'steer1' was dropped, 'steer2' survives (DropOldest)"
    );
    assert_eq!(
        e.history.0.len(),
        4,
        "History: user + assistant(tool_use) + one user(tool_result) + assistant(final)"
    );

    // --- Build the on-disk-shaped log carrying BOTH strata --------------------
    // Append every stratum-1 Event AND the genuine stratum-2 records the live driver
    // writes (CommandDispatched dispatch-intents + the MessageDropped overflow notice).
    let stratum2 = collect_stratum2(&events, &model, limits).await;
    let mut log = MemoryEventLog::new();
    for ev in &events {
        log.append(ev).expect("append stratum-1");
    }
    for record in &stratum2 {
        log.append_lifecycle(record).expect("append stratum-2");
    }

    // The stratum-2 records ARE present (a WAL dispatch-intent AND a drop notice), yet
    // the stratum-1 stream the replay reads is UNTOUCHED by them (two-strata separation).
    let loaded = log.load().expect("load stratum-1");
    assert_eq!(loaded, events, "stratum-2 appends do not pollute the stratum-1 replay stream");
    let lifecycle = log.load_lifecycle().expect("load stratum-2");
    assert!(
        lifecycle
            .iter()
            .any(|r| matches!(r, LifecycleEvent::CommandDispatched { .. })),
        "the log carries a write-ahead CommandDispatched (WAL present)"
    );
    assert!(
        lifecycle.iter().any(|r| matches!(
            r,
            LifecycleEvent::MessageDropped { reason: DropReason::InboxOverflow, .. }
        )),
        "the log carries a MessageDropped overflow notice (stratum-2 present)"
    );

    // --- Replay the stratum-1 stream → BYTE-IDENTICAL to the live fold --------
    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let replay = replay_log(&loaded, &model, limits).expect("replay folds the recorded log");
    let live_bytes = serde_json::to_vec(&live).expect("serialize live World");
    let replay_bytes = serde_json::to_vec(&replay).expect("serialize replay World");
    assert_eq!(
        live_bytes, replay_bytes,
        "replay reconstructs a BYTE-IDENTICAL World despite stratum-2 records in the log (Inv 6)"
    );
    assert_eq!(live, replay, "replay must reconstruct an equal World");

    // The SECOND independent replay is byte-identical to the first (determinism).
    let replay_again = replay_log(&loaded, &model, limits).expect("second replay fold");
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
