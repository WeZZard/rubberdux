//! VC-4.1 / VC-4.2 — multi-hold policy-halt governance (GV-sink).
//!
//! Proves, offline and deterministically, that:
//!
//! - **VC-4.1 (coexistence + reopen-only-when-both-clear)** — a guardrail-tripped
//!   `PolicyHalt(model-refusal)` hold and a concurrent `User` pause hold COEXIST in
//!   `world.resources.gate.holds`; the gate stays Paused (not Open) while EITHER hold
//!   remains; the gate returns to Open ONLY when BOTH clear; the recorded log replays
//!   BYTE-IDENTICALLY (fold == replay, Inv 6).
//!
//! - **VC-4.2 (authority reject/accept)** — a `ClearPolicyHalt` with an INVALID/empty
//!   `Authority` is REJECTED — the targeted hold stays and the World is serde-identical
//!   (byte-identical JSON) before/after the rejected attempt — while a VALID authority
//!   clears EXACTLY the matching `PolicyHalt(model-refusal)` hold (the concurrent `User`
//!   hold survives, keeping the gate Paused).
//!
//! No live model call is made. The refusal is a synthesized
//! `ModelResponded{stop_reason: Refusal}` input. The real `GuardrailSystem` and
//! `GateSystem` (registered in `tick`) drive every assertion.
//!
//! Non-vacuity guarantees:
//! - VC-4.1: `GateSystem.Pause` APPENDS the `User` hold WITHOUT clobbering the
//!   pre-existing `PolicyHalt` — removing that append-not-replace behaviour breaks the
//!   "both holds coexist" assertion; removing the "only-when-both-clear" logic (draining
//!   all holds on `Resume`) breaks the "gate stays Paused after Resume" assertion; a
//!   no-op clear breaks the "gate is Open after ClearPolicyHalt" assertion.
//! - VC-4.2: removing `authority.authorizes_clear()` lets the empty-authority branch
//!   clear the hold, breaking the "World is byte-identical after rejected clear"
//!   assertion; removing the per-source clear logic (draining all holds on any clear)
//!   breaks the "User hold survives the valid clear" assertion.
//!
//! See docs/agent/world/ecs-runtime.md — WorldGate; Invariant 13 (gate-proof settling);
//! §336-356, §419-425, §1857-1882 (multi-hold governance; authority enforcement).

use std::collections::BTreeMap;

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{Command, fingerprint_call};
use rubberdux::agent::world::gates::{Authority, EntityGate, GuardrailTrip, PauseReason};
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::systems::gate::GateSystem;
use rubberdux::agent::world::systems::guardrail::model_refusal_trip;
use rubberdux::agent::world::systems::{System, tick};
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

const SEED: u64 = 42;

// ---------------------------------------------------------------------------
// Harness — genesis, fold, replay, fingerprint-stamp
// ---------------------------------------------------------------------------

fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-policy-halt-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

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
            budget: Budget::default(),
            inbox: Inbox::default(),
            turns: 0,
            spawned: 0,
            model: None,
            autonomy: None,
        },
    );
    world
}

fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

/// The canonical ("live") World: re-apply every recorded Event through `tick`.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    Ok(replay::fold_log(genesis_from_log(events, model)?, events))
}

/// Replay under the cursor-driven driver. Exploding client is structurally unreachable
/// (the replay path never calls the model), proving Inv 6.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(
        genesis_from_log(events, model)?,
        events,
        replay::is_model_call_result,
    )
}

/// Stamp each `ModelResponded`/`ModelFailed`/`InferenceCancelled` event's
/// `fingerprint` with the hash the LIVE driver would have recorded for the
/// request the reducer re-emits for that `cmd`, so replay reuses each result
/// instead of diverging (Inv 7).
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

/// Assert a fold and a cursor-driven replay reconstruct a BYTE-IDENTICAL `World`
/// (Inv 6), and that a second replay is byte-identical too (determinism), and that
/// the model client was never invoked (Inv 6/7/9).
fn assert_replays_byte_identical(events: &[Event], model: &ModelConfig, live: &World) {
    let replay = replay_log(events, model).expect("replay folds the recorded log");
    assert_eq!(
        serde_json::to_vec(live).expect("serialize live World"),
        serde_json::to_vec(&replay).expect("serialize replay World"),
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, &replay, "replay must reconstruct an equal World");
    let replay_again = replay_log(events, model).expect("second replay fold");
    assert_eq!(
        serde_json::to_vec(&replay).expect("serialize first replay"),
        serde_json::to_vec(&replay_again).expect("serialize second replay"),
        "two independent replays of the same log are byte-identical (determinism)"
    );
    // Replay invokes the model client zero times by construction: replay_world /
    // drive_replay take no ModelCaller, so the recorded results are reused, never
    // re-fetched (Inv 6).
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

/// A synthesized `ModelResponded` with `stop_reason: Refusal` — the concrete
/// policy-predicate match that `GuardrailSystem` acts on. Fingerprint is a
/// placeholder filled by `stamp_fingerprints`.
fn model_responded_refusal(at: u64, cmd: CmdId) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            blocks: vec![Block::Text {
                text: "I cannot help with that.".into(),
            }],
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-policy-halt-replay".into(),
                stop_reason: StopReason::Refusal,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

fn pause_user(at: u64) -> Event {
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

fn clear_policy_halt(at: u64, trip: GuardrailTrip, authority: &str) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ClearPolicyHalt {
            trip,
            authority: Authority(authority.into()),
        },
    }
}

// ---------------------------------------------------------------------------
// VC-4.1 — coexistence + reopen-only-when-both-clear + byte-identical replay
// ---------------------------------------------------------------------------

/// **VC-4.1** A guardrail-tripped `PolicyHalt(model-refusal)` hold and a concurrent
/// `User` pause hold COEXIST in `world.resources.gate.holds`; the gate stays Paused
/// while EITHER hold remains; the gate returns to Open ONLY when BOTH clear; the
/// recorded log replays BYTE-IDENTICALLY (fold == replay, Inv 6).
///
/// Log structure (all offline, no live model call):
///
/// ```text
/// 0: session_started
/// 1: user_message("dangerous request")   → CallModel(cmd=0), entity→Thinking
/// 2: model_responded(cmd=0, Refusal)     → GuardrailSystem adds PolicyHalt hold;
///                                           TurnSystem settles entity→Idle (gate-proof, Inv 13)
/// 3: pause(User)                         → GateSystem adds User hold; both holds coexist
/// 4: resume                              → GateSystem drains User holds only; PolicyHalt survives
/// 5: clear_policy_halt(model-refusal,"admin") → GateSystem removes PolicyHalt; gate Open
/// ```
///
/// Non-vacuity: removing coexistence logic (replacing vs. appending) breaks assertion (b);
/// removing per-source clearing (Resume draining ALL holds) breaks assertion (c);
/// removing the ClearPolicyHalt fold breaks assertion (d).
#[test]
fn vc_4_1_coexisting_holds_gate_reopens_only_when_both_clear() {
    let model = offline_model();
    let mut events = vec![
        session_started(),
        user_message(1, "dangerous request"),
        model_responded_refusal(2, 0),
        pause_user(3),
        resume(4),
        clear_policy_halt(5, model_refusal_trip(), "admin"),
    ];
    stamp_fingerprints(&mut events, &model);

    // Fold step-by-step to observe each transition.
    let mut world = genesis_from_log(&events, &model).expect("genesis");
    let mut after_refusal: Option<World> = None;
    let mut after_pause: Option<World> = None;
    let mut after_resume: Option<World> = None;

    for ev in &events {
        let (next, _cmds) = tick(&world, ev);
        world = next.clone();
        match ev.at {
            2 => after_refusal = Some(world.clone()),
            3 => after_pause = Some(world.clone()),
            4 => after_resume = Some(world.clone()),
            _ => {}
        }
    }

    // (a) After the refusal: exactly ONE PolicyHalt hold; gate Paused.
    //     The TurnSystem settled the entity gate-proof (Inv 13): entity is Idle.
    let wr = after_refusal.expect("observed tick 2");
    assert_eq!(
        wr.resources.gate.holds,
        vec![PauseReason::PolicyHalt(model_refusal_trip())],
        "a Refusal trips exactly one PolicyHalt(model-refusal) hold"
    );
    assert!(
        !wr.resources.gate.is_open(),
        "gate is Paused by the PolicyHalt hold alone"
    );
    assert!(
        matches!(
            wr.entities.get(&0).expect("primary entity").activity,
            Activity::Idle
        ),
        "the Refusal settled gate-proof: entity is Idle (Inv 13)"
    );

    // (b) After the user pause: BOTH holds COEXIST; gate still Paused.
    //     NON-VACUOUS: if Pause replaced the PolicyHalt instead of appending,
    //     holds would have only one entry here.
    let wp = after_pause.expect("observed tick 3");
    assert!(
        wp.resources.gate.holds.contains(&PauseReason::PolicyHalt(model_refusal_trip())),
        "PolicyHalt hold SURVIVES the concurrent User pause (coexistence — GV-trip)"
    );
    assert!(
        wp.resources.gate.holds.contains(&PauseReason::User),
        "User hold was ADDED alongside the PolicyHalt hold (coexistence — GV-authority)"
    );
    assert_eq!(
        wp.resources.gate.holds.len(),
        2,
        "exactly two holds coexist: PolicyHalt + User (multi-hold)"
    );
    assert!(
        !wp.resources.gate.is_open(),
        "gate is Paused while both holds remain"
    );

    // (c) After resume: User hold cleared; PolicyHalt SURVIVES; gate still Paused.
    //     NON-VACUOUS: if Resume drained ALL holds, the gate would be Open here.
    let wr2 = after_resume.expect("observed tick 4");
    assert_eq!(
        wr2.resources.gate.holds,
        vec![PauseReason::PolicyHalt(model_refusal_trip())],
        "Resume drains User holds ONLY; PolicyHalt(model-refusal) survives"
    );
    assert!(
        !wr2.resources.gate.is_open(),
        "gate is STILL Paused after Resume — the PolicyHalt keeps it closed"
    );

    // (d) Final state (after ClearPolicyHalt): all holds cleared; gate Open.
    //     NON-VACUOUS: if ClearPolicyHalt were a no-op, holds would still contain
    //     the PolicyHalt and the gate would still be closed.
    assert!(
        world.resources.gate.holds.is_empty(),
        "ClearPolicyHalt with valid authority drained the last hold"
    );
    assert!(
        world.resources.gate.is_open(),
        "gate is Open ONLY after BOTH holds have cleared (VC-4.1 core invariant)"
    );
    assert!(
        matches!(
            world.entities.get(&0).expect("primary entity").activity,
            Activity::Idle
        ),
        "entity remains Idle throughout — no new turn was initiated while gated"
    );

    // (e) Byte-identical replay (Inv 6).
    let live = fold_log(&events, &model).expect("the log folds");
    assert_replays_byte_identical(&events, &model, &live);
}

// ---------------------------------------------------------------------------
// VC-4.2 — authority reject/accept + World byte-identity on rejection
// ---------------------------------------------------------------------------

/// **VC-4.2** A `ClearPolicyHalt` with an INVALID/empty authority is REJECTED: the
/// targeted hold stays and the World is byte-identical (serde-equal JSON) before/after
/// the rejected attempt. A VALID authority clears EXACTLY the one matching
/// `PolicyHalt(model-refusal)` hold; the concurrent `User` hold survives, so the gate
/// stays Paused.
///
/// Setup: events 0-3 (same prefix as VC-4.1) fold to a state with both
/// `PolicyHalt(model-refusal)` and `User` holds. The three authority probes are
/// applied OUTSIDE the recorded log (a single tick from the folded World), so the
/// byte-identical replay in part (e) covers only the canonical four events.
///
/// Non-vacuity: removing `Authority::authorizes_clear()` lets the empty-authority
/// attempt clear the hold, breaking assertion (a); removing per-source clear logic
/// breaks assertion (c) ("User hold survives the valid clear").
#[test]
fn vc_4_2_invalid_authority_rejected_valid_authority_clears_exactly_one_hold() {
    let model = offline_model();
    // Setup: same prefix as VC-4.1, stopped after the user pause so both holds are
    // present at the fold boundary.
    //   0: session_started
    //   1: user_message  → CallModel(cmd=0)
    //   2: model_responded(Refusal) → PolicyHalt hold added
    //   3: pause(User)   → User hold added
    let mut events = vec![
        session_started(),
        user_message(1, "forbidden request"),
        model_responded_refusal(2, 0),
        pause_user(3),
    ];
    stamp_fingerprints(&mut events, &model);

    // Fold to the state with both holds.
    let world_both = fold_log(&events, &model).expect("fold to both-holds state");
    assert_eq!(
        world_both.resources.gate.holds.len(),
        2,
        "setup: two holds present (PolicyHalt + User)"
    );
    assert!(
        !world_both.resources.gate.is_open(),
        "gate closed by both holds (setup check)"
    );

    // Canonical JSON snapshot BEFORE any clear attempt — the baseline for the
    // byte-identity proof.
    //
    // We call `GateSystem.step()` DIRECTLY (not `tick()`) for the authority
    // rejection checks, mirroring the gate.rs unit test. `tick()` always advances
    // `world.clock` to the event's `at` tick, so a comparison between a
    // tick-3 World and a tick-4 World would differ on the clock field even when the
    // fold is a no-op. `GateSystem.step()` is the fold that owns the `ClearPolicyHalt`
    // input — it is the real production System, not a stub — so using it here still
    // exercises the actual authority enforcement logic.
    let before_json = serde_json::to_vec(&world_both).expect("serialize World before clears");

    // --- (a) Empty authority: REJECTED; World is BYTE-IDENTICAL ---
    //
    // NON-VACUOUS: if `Authority::authorizes_clear()` were absent (always true),
    // this step would clear the PolicyHalt hold and the holds vector would shrink
    // to 1, failing the byte-identity assertion.
    let (after_empty, cmds_empty) = GateSystem.step(
        &world_both,
        &LogicalInput::ClearPolicyHalt {
            trip: model_refusal_trip(),
            authority: Authority(String::new()),
        },
    );
    assert!(
        cmds_empty.is_empty(),
        "a rejected clear emits no Commands"
    );
    assert_eq!(
        after_empty,
        world_both,
        "an insufficient authority leaves the World STRUCTURALLY unchanged (VC-4.2 negative)"
    );
    assert_eq!(
        serde_json::to_vec(&after_empty).expect("serialize World after empty-authority clear"),
        before_json,
        "an empty authority leaves the World BYTE-IDENTICAL (VC-4.2 negative)"
    );
    assert_eq!(
        after_empty.resources.gate.holds.len(),
        2,
        "both holds remain after empty-authority rejected clear"
    );
    assert!(
        !after_empty.resources.gate.is_open(),
        "gate stays Paused — rejected clear has no effect"
    );
    assert!(
        after_empty.resources.gate.holds.contains(&PauseReason::PolicyHalt(model_refusal_trip())),
        "PolicyHalt hold stays after rejected empty-authority clear"
    );
    assert!(
        after_empty.resources.gate.holds.contains(&PauseReason::User),
        "User hold stays after rejected empty-authority clear"
    );

    // --- (b) Whitespace authority: also REJECTED; also BYTE-IDENTICAL ---
    let (after_ws, cmds_ws) = GateSystem.step(
        &world_both,
        &LogicalInput::ClearPolicyHalt {
            trip: model_refusal_trip(),
            authority: Authority("   ".into()),
        },
    );
    assert!(cmds_ws.is_empty(), "a whitespace-authority rejected clear emits no Commands");
    assert_eq!(
        after_ws,
        world_both,
        "a whitespace authority is also rejected — World is STRUCTURALLY unchanged"
    );
    assert_eq!(
        serde_json::to_vec(&after_ws).expect("serialize World after whitespace-authority clear"),
        before_json,
        "a whitespace authority is also rejected — World is BYTE-IDENTICAL"
    );
    assert_eq!(
        after_ws.resources.gate.holds.len(),
        2,
        "both holds remain after whitespace-authority rejected clear"
    );

    // --- (c) Valid authority: clears EXACTLY the PolicyHalt hold; User survives;
    //         gate stays Paused ---
    //
    // NON-VACUOUS: if the clear drained all holds (not per-source), holds would be
    // empty and the gate would be Open, breaking "User hold survives".
    let (after_valid, cmds_valid) = GateSystem.step(
        &world_both,
        &LogicalInput::ClearPolicyHalt {
            trip: model_refusal_trip(),
            authority: Authority("admin-token".into()),
        },
    );
    assert!(cmds_valid.is_empty(), "a valid clear emits no Commands");
    assert_eq!(
        after_valid.resources.gate.holds,
        vec![PauseReason::User],
        "a valid authority removes EXACTLY the PolicyHalt hold; the User hold survives (VC-4.2)"
    );
    assert!(
        !after_valid.resources.gate.is_open(),
        "gate stays Paused — the surviving User hold keeps it closed"
    );
    assert!(
        !after_valid
            .resources
            .gate
            .holds
            .contains(&PauseReason::PolicyHalt(model_refusal_trip())),
        "PolicyHalt(model-refusal) was removed by the valid authority"
    );
    assert!(
        after_valid.resources.gate.holds.contains(&PauseReason::User),
        "User hold survived the PolicyHalt clear (per-source clearing)"
    );

    // --- (d) Byte-identical replay of the four-event canonical log (Inv 6) ---
    //
    // The authority probes were applied OUTSIDE the recorded log (directly from
    // the folded World), so this replay covers only events 0..3 and is
    // independent of the probe results.
    let live = fold_log(&events, &model).expect("the four-event log folds");
    assert_replays_byte_identical(&events, &model, &live);
}
