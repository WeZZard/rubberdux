//! VC-1.2 / VC-1.3 / VC-1.6 / VC-1.7 — pause, cancel, human-action / approval, and
//! steering as hand-authored synthetic logs that fold through the pure `tick` reducer
//! and then REPLAY to a byte-identical `World` with zero model/tool re-invocation (the
//! Phase-1a determinism core).
//!
//! This file is pure composition — it touches no `src/agent/world/*` file. Each behavior
//! is a hand-authored log, folded + asserted, then replayed byte-identical (Inv 6/7/9):
//!
//! - **VC-1.2 (pause)** — while paused, a result that completes the turn STILL settles its
//!   slot (gate-proof, Inv 13) but the turn-advance continuation is DEFERRED; once a
//!   `Resume` reopens the gate a later result fires the continuation.
//! - **VC-1.3 (cancel)** — `Cancel` during `ResolvingToolUses` owes every in-flight `cmd`,
//!   emits the aborts, and ABSORBS each `ToolAborted` ack until the entity reaches `Idle`
//!   (Inv 10, 14).
//! - **VC-1.6 (human-action / approval)** — a `Human` slot resolves via `HumanActionDone`
//!   (`Provided` → result; `Declined` → `is_error`, Inv 10); a gated `Local` slot resolves
//!   via `RaiseInteraction` ↔ `InteractionAnswer` (`Accepted` → run; `Rejected` →
//!   `is_error`, Inv 10).
//! - **VC-1.7 (steer)** — a `UserMessage` arriving MID-RUN enqueues on the entity's Inbox
//!   (it never interrupts the in-flight turn) and is honoured on the next turn initiation.
//!
//! Every log makes NO live model call, so the file needs no credentials and runs on every
//! developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), 7 (fingerprint),
//! 9 (log = single source of truth), 10 (totality), 13 (gate-proof settling), 14
//! (`Cancelling` absorber), 16 (slot identity).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::autonomy::{Autonomy, InteractionResponse};
use rubberdux::agent::world::effects::{Command, ModelCaller, fingerprint_call};
use rubberdux::agent::world::gates::{EntityGate, PauseReason};
use rubberdux::agent::world::history::{Block, History, Role};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, HumanResult, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
    StopReason, Usage,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, EntityId, Identity, Lineage, ModelConfig, ReqId, Resources,
    SlotState, World, HELD_CMD,
};
use rubberdux::error::Error;

const SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Genesis / fold / replay / fingerprint-stamp harness
// ---------------------------------------------------------------------------

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

fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-human-and-cancel-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// A `ModelCaller` that records any invocation and then panics. `drive_replay` takes NO
/// client, so this can never be threaded into `replay_log` by construction; asserting its
/// counter stays zero makes the "zero model/tool calls" guarantee (Inv 6) explicit.
struct ExplodingClient {
    calls: Arc<AtomicUsize>,
}

impl ModelCaller for ExplodingClient {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("the replay driver must never invoke the model client");
    }
}

/// The canonical ("live") World: re-apply every recorded Event through `tick`.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    Ok(replay::fold_log(genesis_from_log(events, model)?, events))
}

/// Replay under the promoted cursor-driven driver (`replay::replay_world`). The
/// `ReplayCursor` stands in for the model-call results ALONE
/// (`replay::is_model_call_result`), so a continuation `CallModel` re-emitted after a
/// `ToolReturned` / `HumanActionDone` pulls the NEXT `ModelResponded` from the cursor —
/// never a tool/human result. Every OTHER recorded input — the exogenous free variables
/// (a mid-run `UserMessage`, `Pause`/`Resume`, `Cancel`, `SetAutonomy`,
/// `InteractionAnswer`) AND the non-model results (`ToolReturned`, `ToolAborted`,
/// `HumanActionDone`) — is re-applied directly. A `Diverged` outcome is a replay failure.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(
        genesis_from_log(events, model)?,
        events,
        replay::is_model_call_result,
    )
}

/// Stamp each model-call result's `fingerprint` with the value the LIVE driver would have
/// recorded (the hash of the request the reducer re-emits for that `cmd`), so the replay
/// reuses each result instead of diverging (Inv 7) — without hand-authoring any
/// continuation `History`.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig) {
    let mut world = genesis_from_log(events, model).expect("genesis for fingerprints");
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

/// Replay the log to a byte-identical `World` (Inv 6) and assert the model client was
/// never invoked (Inv 6/7/9). Shared across every behavior in this file.
fn assert_replays_byte_identical(events: &[Event], model: &ModelConfig, live: &World) {
    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let replay = replay_log(events, model).expect("replay folds the recorded log");
    assert_eq!(
        serde_json::to_vec(live).expect("serialize live World"),
        serde_json::to_vec(&replay).expect("serialize replay World"),
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, &replay, "replay must reconstruct an equal World");
    // Two independent replays are byte-identical (determinism).
    let replay_again = replay_log(events, model).expect("second replay fold");
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

/// A `ModelResponded` for the primary entity. Fingerprint is a placeholder filled by
/// `stamp_fingerprints`.
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
                model_id: "claude-human-and-cancel-replay".into(),
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

fn tool_aborted(at: u64, cmd: CmdId) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ToolAborted { cmd, entity: 0 },
    }
}

fn pause(at: u64) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::Pause {
            reason: PauseReason::User,
        },
    }
}

fn resume(at: u64) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::Resume,
    }
}

fn cancel(at: u64, entity: EntityId) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::Cancel { entity },
    }
}

fn set_autonomy(at: u64, policy: Autonomy) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::SetAutonomy { policy },
    }
}

fn human_action_done(at: u64, cmd: CmdId, result: HumanResult) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::HumanActionDone {
            cmd,
            entity: 0,
            fingerprint: Fingerprint("fp-human-result".into()),
            result,
        },
    }
}

fn interaction_answer(at: u64, request_id: ReqId, answer: InteractionResponse) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::InteractionAnswer { request_id, answer },
    }
}

/// A `ToolUse` block resolved by a `Local` tool (`RunTool`).
fn local_tool_use(id: &str, name: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input: serde_json::json!({}),
    }
}

/// A `ToolUse` block routed to the `Human` slot kind (`ask_human` → `RequestHumanAction`).
fn ask_human_use(id: &str, prompt: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: "ask_human".into(),
        input: serde_json::json!({ "text": prompt }),
    }
}

/// The single primary entity's `Activity` after folding a prefix of `events`.
fn activity_of(world: &World) -> &Activity {
    &world.entities.get(&0).expect("primary entity present").activity
}

// ---------------------------------------------------------------------------
// VC-1.2 — pause defers the continuation; resume lets it proceed
// ---------------------------------------------------------------------------

/// **VC-1.2** While paused, a `ToolReturned` that completes the LAST slot STILL settles
/// it (gate-proof, Inv 13), yet — every slot now `Done` — the turn-advance continuation is
/// DEFERRED because the gate is closed. The entity waits in `ResolvingToolUses`. Then
/// replays byte-identical (Inv 6).
#[test]
fn vc_1_2_pause_defers_the_all_done_continuation() {
    let model = offline_model();
    let mut events = vec![
        session_started(),
        user_message(1, "do one thing"),
        // The single-slot tool turn dispatches its RunTool (cmd 1) BEFORE the pause.
        model_responded(2, 0, vec![local_tool_use("tu_x", "tool_x")], StopReason::ToolUse),
        pause(3),
        // The only slot's result lands WHILE PAUSED: it settles (gate-proof, Inv 13) and
        // all slots are now Done — but the continuation is deferred (the gate is closed).
        tool_returned(4, 1, "result X"),
    ];
    stamp_fingerprints(&mut events, &model);

    // Fold step by step so the paused tick's Commands are observable.
    let mut world = genesis_from_log(&events, &model).expect("genesis");
    let mut paused_tick_commands = Vec::new();
    for ev in &events {
        let (next, commands) = tick(&world, ev);
        world = next;
        if matches!(ev.input, LogicalInput::ToolReturned { .. }) {
            paused_tick_commands = commands;
        }
    }

    // The completing result emitted NO continuation while paused.
    assert!(
        !paused_tick_commands
            .iter()
            .any(|c| matches!(c, Command::CallModel { .. })),
        "the continuation is DEFERRED while paused (Inv 13)"
    );
    // The slot still settled, and the entity waits at the all-Done boundary, gate closed.
    match activity_of(&world) {
        Activity::ResolvingToolUses { slots } => {
            assert!(
                slots.iter().all(|s| matches!(s.state, SlotState::Done)),
                "the in-flight result STILL settled its slot while paused (gate-proof, Inv 13)"
            );
        }
        other => panic!("the entity waits in ResolvingToolUses, got {other:?}"),
    }
    assert!(
        !world.resources.gate.is_open(),
        "the gate is still closed — the pause hold survives result-settling"
    );

    let live = fold_log(&events, &model).expect("the paused log folds");
    assert_replays_byte_identical(&events, &model, &live);
}

/// **VC-1.2** With two slots, the FIRST result settles while paused; a `Resume` reopens
/// the gate; the SECOND result then completes the turn and the continuation fires
/// (`→ Thinking`), and a final `EndTurn` settles the entity `→ Idle`. Then replays
/// byte-identical (Inv 6).
#[test]
fn vc_1_2_resume_lets_the_deferred_continuation_proceed() {
    let model = offline_model();
    let mut events = vec![
        session_started(),
        user_message(1, "do two things"),
        // Two Local slots; both RunTools (cmd 1 = tu_a, cmd 2 = tu_b) dispatch pre-pause.
        model_responded(
            2,
            0,
            vec![local_tool_use("tu_a", "tool_a"), local_tool_use("tu_b", "tool_b")],
            StopReason::ToolUse,
        ),
        pause(3),
        // tu_a settles WHILE PAUSED (gate-proof); tu_b is still pending → no continuation.
        tool_returned(4, 1, "result A"),
        resume(5),
        // tu_b settles after Resume → all Done + gate open → continuation (cmd 3).
        tool_returned(6, 2, "result B"),
        model_responded(7, 3, vec![Block::Text { text: "both done".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut events, &model);

    // Fold step by step to observe the paused settle and the post-Resume continuation.
    let mut world = genesis_from_log(&events, &model).expect("genesis");
    let mut after_paused_settle: Option<World> = None;
    let mut post_resume_commands = Vec::new();
    for ev in &events {
        let (next, commands) = tick(&world, ev);
        world = next;
        match &ev.input {
            // The tu_a result lands at tick 4 (paused).
            LogicalInput::ToolReturned { cmd: 1, .. } => after_paused_settle = Some(world.clone()),
            // The tu_b result lands at tick 6 (after Resume).
            LogicalInput::ToolReturned { cmd: 2, .. } => post_resume_commands = commands,
            _ => {}
        }
    }

    // While paused, tu_a settled but the entity stayed in ResolvingToolUses (no continuation).
    let paused = after_paused_settle.expect("the paused settle was observed");
    match activity_of(&paused) {
        Activity::ResolvingToolUses { slots } => {
            let tu_a = slots.iter().find(|s| s.tool_use_id == "tu_a").expect("tu_a slot");
            assert!(
                matches!(tu_a.state, SlotState::Done),
                "tu_a settled WHILE PAUSED (gate-proof, Inv 13)"
            );
        }
        other => panic!("expected ResolvingToolUses while paused, got {other:?}"),
    }
    assert!(
        !paused.resources.gate.is_open(),
        "the gate was closed while tu_a settled"
    );

    // After Resume, completing the turn fired exactly one continuation CallModel.
    assert_eq!(
        post_resume_commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { .. }))
            .count(),
        1,
        "Resume reopened the gate so the deferred continuation proceeds"
    );

    let live = fold_log(&events, &model).expect("the resume log folds");
    assert!(
        matches!(activity_of(&live), Activity::Idle),
        "the turn completes to Idle once resumed"
    );
    assert!(
        live.resources.gate.is_open(),
        "the gate is open after Resume"
    );
    assert_replays_byte_identical(&events, &model, &live);
}

// ---------------------------------------------------------------------------
// VC-1.3 — Cancel during ResolvingToolUses absorbs every owed ack → Idle
// ---------------------------------------------------------------------------

/// **VC-1.3** `Cancel` during `ResolvingToolUses` owes every in-flight `cmd`, emits one
/// abort per slot, then ABSORBS each `ToolAborted` ack one by one until the entity reaches
/// `Idle` (Inv 10, 14). Then replays byte-identical (Inv 6).
#[test]
fn vc_1_3_cancel_resolving_absorbs_acks_to_idle() {
    let model = offline_model();
    let mut events = vec![
        session_started(),
        user_message(1, "two tools then cancel"),
        // Two Local slots → RunTool cmd 1 (tu_a) and cmd 2 (tu_b) in flight.
        model_responded(
            2,
            0,
            vec![local_tool_use("tu_a", "tool_a"), local_tool_use("tu_b", "tool_b")],
            StopReason::ToolUse,
        ),
        // Cancel owes both cmds → Cancelling { awaiting: [1, 2] }, two CancelTool aborts.
        cancel(3, 0),
        // The two abort acks are absorbed one by one → Idle.
        tool_aborted(4, 1),
        tool_aborted(5, 2),
    ];
    stamp_fingerprints(&mut events, &model);

    // Fold step by step to observe the Cancel transition and the absorbed acks.
    let mut world = genesis_from_log(&events, &model).expect("genesis");
    let mut cancel_commands = Vec::new();
    let mut after_first_ack: Option<World> = None;
    for ev in &events {
        let (next, commands) = tick(&world, ev);
        world = next;
        match &ev.input {
            LogicalInput::Cancel { .. } => cancel_commands = commands,
            LogicalInput::ToolAborted { cmd: 1, .. } => after_first_ack = Some(world.clone()),
            _ => {}
        }
    }

    // After both acks are absorbed, the fully-folded entity is Idle (Inv 14).
    assert_eq!(
        activity_of(&world),
        &Activity::Idle,
        "after both acks are absorbed the entity is Idle (Inv 14)"
    );
    // Cancel entered Cancelling owing both cmds and emitted one CancelTool per slot.
    assert_eq!(
        cancel_commands
            .iter()
            .filter(|c| matches!(c, Command::CancelTool { .. }))
            .count(),
        2,
        "Cancel emits one CancelTool abort per Pending Local slot"
    );

    // The first ack cleared cmd 1, leaving exactly cmd 2 still owed.
    let mid = after_first_ack.expect("the first ack was observed");
    assert_eq!(
        activity_of(&mid),
        &Activity::Cancelling { awaiting: vec![2] },
        "the first ToolAborted absorbs cmd 1; cmd 2 is still owed"
    );

    let live = fold_log(&events, &model).expect("the cancel log folds");
    let e = live.entities.get(&0).expect("entity");
    assert!(
        matches!(e.activity, Activity::Idle),
        "absorbing every owed ack settles the entity to Idle (Inv 14)"
    );
    // No tool_result was folded — the turn was cancelled, leaving user + assistant(tool_use).
    assert_eq!(
        e.history.0.len(),
        2,
        "a cancelled turn folds no tool_result (user + assistant(tool_use) only)"
    );
    assert_replays_byte_identical(&events, &model, &live);
}

// ---------------------------------------------------------------------------
// VC-1.6 — human action: Provided → result, Declined → is_error
// ---------------------------------------------------------------------------

/// **VC-1.6** A `Human` slot emits one `RequestHumanAction`; `HumanActionDone::Provided`
/// resolves it with the JSON answer (non-error) and the turn continues, while
/// `HumanActionDone::Declined` resolves it `is_error` without deadlocking (Inv 10). Each
/// folds and replays byte-identical (Inv 6).
#[test]
fn vc_1_6_human_action_provided_and_declined() {
    let model = offline_model();

    // --- Provided -----------------------------------------------------------------------
    let mut provided = vec![
        session_started(),
        user_message(1, "ask the human"),
        model_responded(
            2,
            0,
            vec![ask_human_use("tu_h", "Confirm?")],
            StopReason::ToolUse,
        ),
        human_action_done(3, 1, HumanResult::Provided(serde_json::json!("yes"))),
        model_responded(4, 2, vec![Block::Text { text: "acknowledged".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut provided, &model);

    // The Human slot emits exactly one RequestHumanAction.
    let mut world = genesis_from_log(&provided, &model).expect("genesis");
    let mut request_commands = Vec::new();
    for ev in &provided {
        let (next, commands) = tick(&world, ev);
        world = next;
        if matches!(ev.input, LogicalInput::ModelResponded { cmd: 0, .. }) {
            request_commands = commands;
        }
    }
    assert_eq!(
        request_commands
            .iter()
            .filter(|c| matches!(c, Command::RequestHumanAction { .. }))
            .count(),
        1,
        "a Human slot emits exactly one RequestHumanAction"
    );

    let live = fold_log(&provided, &model).expect("the provided log folds");
    let e = live.entities.get(&0).expect("entity");
    assert!(matches!(e.activity, Activity::Idle), "the turn completed to Idle");
    match &e.history.0[2].content[0] {
        Block::ToolResult {
            is_error, content, ..
        } => {
            assert!(!is_error, "a Provided human result is NOT is_error");
            assert!(
                matches!(content.as_slice(), [Block::Text { text }] if text.contains("yes")),
                "the human's JSON answer rides in the tool_result"
            );
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
    assert_replays_byte_identical(&provided, &model, &live);

    // --- Declined -----------------------------------------------------------------------
    let mut declined = vec![
        session_started(),
        user_message(1, "ask the human"),
        model_responded(
            2,
            0,
            vec![ask_human_use("tu_h", "Confirm?")],
            StopReason::ToolUse,
        ),
        human_action_done(3, 1, HumanResult::Declined),
        model_responded(4, 2, vec![Block::Text { text: "understood".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut declined, &model);

    let live = fold_log(&declined, &model).expect("the declined log folds");
    let e = live.entities.get(&0).expect("entity");
    assert!(
        matches!(e.activity, Activity::Idle),
        "Declined → is_error → turn continues, no deadlock (Inv 10)"
    );
    match &e.history.0[2].content[0] {
        Block::ToolResult { is_error, .. } => {
            assert!(*is_error, "a Declined human action rides as is_error (Inv 10)")
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
    assert_replays_byte_identical(&declined, &model, &live);
}

// ---------------------------------------------------------------------------
// VC-1.6 — approval: RaiseInteraction ↔ InteractionAnswer (Accepted / Rejected)
// ---------------------------------------------------------------------------

/// **VC-1.6** Under a gating `Autonomy` policy a `Local` slot is HELD and a
/// `RaiseInteraction` is emitted; `InteractionAnswer::Accepted` releases the slot (the
/// `RunTool` runs and the turn continues), while `InteractionAnswer::Rejected` resolves it
/// `is_error` without deadlocking (Inv 10). Each folds and replays byte-identical (Inv 6).
#[test]
fn vc_1_6_interaction_approval_accept_and_reject() {
    let model = offline_model();
    // The first minted request_id is 0 (the SOLE id minter starts there), so the answer
    // references request 0.
    const REQUEST_ID: ReqId = 0;

    // --- Accepted -----------------------------------------------------------------------
    let mut accepted = vec![
        session_started(),
        set_autonomy(1, Autonomy::AskEverything),
        user_message(2, "delete the file"),
        model_responded(
            3,
            0,
            vec![local_tool_use("tu_1", "delete_file")],
            StopReason::ToolUse,
        ),
        interaction_answer(4, REQUEST_ID, InteractionResponse::Accepted),
        tool_returned(5, 1, "deleted /tmp/x"),
        model_responded(6, 2, vec![Block::Text { text: "done".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut accepted, &model);

    // The gating policy HELD the slot and raised an interaction (no RunTool yet).
    let mut world = genesis_from_log(&accepted, &model).expect("genesis");
    let mut gate_commands = Vec::new();
    let mut release_commands = Vec::new();
    for ev in &accepted {
        let (next, commands) = tick(&world, ev);
        world = next;
        match &ev.input {
            LogicalInput::ModelResponded { cmd: 0, .. } => gate_commands = commands,
            LogicalInput::InteractionAnswer { .. } => release_commands = commands,
            _ => {}
        }
    }
    assert!(
        gate_commands
            .iter()
            .any(|c| matches!(c, Command::RaiseInteraction { .. })),
        "a gating policy raises an interaction"
    );
    assert!(
        !gate_commands
            .iter()
            .any(|c| matches!(c, Command::RunTool { .. })),
        "the Local slot is HELD — no RunTool before approval"
    );
    // Approval released the held slot: a RunTool was emitted.
    assert!(
        release_commands
            .iter()
            .any(|c| matches!(c, Command::RunTool { .. })),
        "InteractionAnswer::Accepted releases the held slot (RunTool emitted)"
    );

    let live = fold_log(&accepted, &model).expect("the accepted log folds");
    let e = live.entities.get(&0).expect("entity");
    assert!(matches!(e.activity, Activity::Idle), "the approved turn completed to Idle");
    assert!(
        live.resources.raised.is_empty(),
        "the raised interaction was consumed on answer"
    );
    match &e.history.0[2].content[0] {
        Block::ToolResult { is_error, .. } => {
            assert!(!is_error, "the approved tool_result is not an error")
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
    // A HELD_CMD sentinel never leaks into the settled World.
    assert!(
        !matches!(activity_of(&live), Activity::ResolvingToolUses { .. }),
        "no slot is left held — the turn settled"
    );
    let _ = HELD_CMD;
    assert_replays_byte_identical(&accepted, &model, &live);

    // --- Rejected -----------------------------------------------------------------------
    let mut rejected = vec![
        session_started(),
        set_autonomy(1, Autonomy::AskEverything),
        user_message(2, "delete the file"),
        model_responded(
            3,
            0,
            vec![local_tool_use("tu_1", "delete_file")],
            StopReason::ToolUse,
        ),
        interaction_answer(4, REQUEST_ID, InteractionResponse::Rejected),
        model_responded(5, 1, vec![Block::Text { text: "understood".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut rejected, &model);

    let live = fold_log(&rejected, &model).expect("the rejected log folds");
    let e = live.entities.get(&0).expect("entity");
    assert!(
        matches!(e.activity, Activity::Idle),
        "Rejected → is_error → turn continues, no deadlock (Inv 10)"
    );
    assert!(live.resources.raised.is_empty(), "the raised interaction was consumed");
    match &e.history.0[2].content[0] {
        Block::ToolResult { is_error, .. } => {
            assert!(*is_error, "a rejected interaction resolves the slot is_error (Inv 10)")
        }
        other => panic!("expected a ToolResult, got {other:?}"),
    }
    assert_replays_byte_identical(&rejected, &model, &live);
}

// ---------------------------------------------------------------------------
// VC-1.7 — a mid-run UserMessage enqueues and is honoured on the next turn
// ---------------------------------------------------------------------------

/// **VC-1.7** A `UserMessage` arriving while the entity is MID-RUN (`ResolvingToolUses`)
/// does NOT interrupt the in-flight turn — it PARKS on the entity's Inbox — and is honoured
/// on the next turn the entity initiates from `Idle` (drained FIFO ahead of the triggering
/// message). Then replays byte-identical (Inv 6).
#[test]
fn vc_1_7_mid_run_user_message_enqueues_and_is_honoured_next_turn() {
    let model = offline_model();
    let mut events = vec![
        session_started(),
        user_message(1, "first task"),
        // A tool turn: the entity enters ResolvingToolUses (mid-run) awaiting the result.
        model_responded(2, 0, vec![local_tool_use("tu_x", "tool_x")], StopReason::ToolUse),
        // A UserMessage arrives MID-RUN (entity ResolvingToolUses) → parked on the Inbox.
        user_message(3, "steer me"),
        // The tool result completes the turn → continuation (cmd 2).
        tool_returned(4, 1, "result X"),
        model_responded(5, 2, vec![Block::Text { text: "first done".into() }], StopReason::EndTurn),
        // The next turn initiation drains the parked "steer me" ahead of "next task".
        user_message(6, "next task"),
        model_responded(7, 3, vec![Block::Text { text: "second done".into() }], StopReason::EndTurn),
    ];
    stamp_fingerprints(&mut events, &model);

    // Fold step by step to observe the mid-run enqueue and the drained continuation request.
    let mut world = genesis_from_log(&events, &model).expect("genesis");
    let mut after_steer: Option<World> = None;
    let mut steer_commands = Vec::new();
    let mut next_turn_commands = Vec::new();
    for ev in &events {
        let (next, commands) = tick(&world, ev);
        world = next;
        match &ev.input {
            // The mid-run "steer me" message at tick 3.
            LogicalInput::UserMessage { text, .. } if text == "steer me" => {
                after_steer = Some(world.clone());
                steer_commands = commands.clone();
            }
            // The next-turn trigger at tick 6.
            LogicalInput::UserMessage { text, .. } if text == "next task" => {
                next_turn_commands = commands.clone();
            }
            _ => {}
        }
    }

    // The mid-run message did NOT start a turn; it parked on the Inbox; History untouched.
    assert!(
        steer_commands.is_empty(),
        "a mid-run UserMessage emits no Command (the in-flight turn is not interrupted)"
    );
    let steered = after_steer.expect("the steer tick was observed");
    let e = steered.entities.get(&0).expect("entity");
    assert!(
        matches!(e.activity, Activity::ResolvingToolUses { .. }),
        "the in-flight turn is untouched — still resolving its tool"
    );
    assert_eq!(
        e.inbox.pending,
        vec![vec![Block::Text {
            text: "steer me".into()
        }]],
        "the steered message is durably parked on the Inbox (VC-1.7)"
    );

    // The next turn initiation honoured the parked message ahead of the triggering one.
    let texts: Vec<String> = match next_turn_commands
        .iter()
        .find(|c| matches!(c, Command::CallModel { .. }))
        .expect("the next turn initiates one CallModel")
    {
        Command::CallModel { messages, .. } => messages
            .0
            .iter()
            .filter_map(|m| match (m.role, m.content.as_slice()) {
                (Role::User, [Block::Text { text }]) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => unreachable!(),
    };
    assert!(
        texts.windows(2).any(|w| w == ["steer me", "next task"]),
        "the parked 'steer me' is honoured FIFO, immediately ahead of 'next task' \
         (got user texts {texts:?})"
    );

    let live = fold_log(&events, &model).expect("the steer log folds");
    let e = live.entities.get(&0).expect("entity");
    assert!(matches!(e.activity, Activity::Idle), "both turns completed to Idle");
    assert!(e.inbox.pending.is_empty(), "the Inbox was drained on the next initiation");
    assert!(
        e.history.0.iter().any(|m| matches!(m.role, Role::User)
            && m.content
                .iter()
                .any(|b| matches!(b, Block::Text { text } if text == "steer me"))),
        "the steered message was honoured into History"
    );
    assert_replays_byte_identical(&events, &model, &live);
}
