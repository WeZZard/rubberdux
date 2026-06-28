//! systems — the pure tick reducer and the per-phase Systems.
//! See docs/agent/world/ecs-runtime.md (Tick discipline, Systems).

pub mod intake;
pub mod turn;
pub mod tool;
pub mod human_action;
pub mod interaction;
pub mod steering;
pub mod cancel;
pub mod autonomy;
pub mod gate;
pub mod budget;
pub mod compaction;
pub mod subagent;
pub mod supervision;
pub mod peer;
pub mod surface;

use crate::agent::world::effects::Command;
use crate::agent::world::inputs::{Event, LogicalInput};
use crate::agent::world::world::World;

// ---------------------------------------------------------------------------
// System — a pure reducer over one tick's input
// ---------------------------------------------------------------------------

/// The one logical input a tick processes. A System is a pure function of the
/// current `World` and this single `Input` — no ambient reads (Inv 1).
pub type Input = LogicalInput;

/// A System is a PURE reducer over one tick's input: `(World, Input) -> (World',
/// Vec<Command>)`. It may read ONLY the `World` and the one `Input` handed to it
/// (no clock, RNG, env, or map-iteration ambient reads — Inv 1), and it emits the
/// Commands it wants dispatched; the tick reducer mints their ordinals.
pub trait System {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>);
}

// ---------------------------------------------------------------------------
// Tick discipline — fixed 6-phase order (Inv 12)
// ---------------------------------------------------------------------------

/// The fixed intra-tick phase order (Inv 12). Exactly one Input is processed per
/// tick and the Systems run in THIS order, so the tick is a scheduler-independent
/// function of `(World, Input)`. Populated so far: phase 2 (Intake), phase 3
/// (Settle), and phase 5 (TurnAdvance — ToolSystem); phases 1/4 remain scaffold for
/// later tasks. Phase 6 (Emit) is the reducer's own ordinal-minting step, below —
/// not a System phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 1 — lifecycle / gate updates (Pause/Resume/ClearHalt). P1a+.
    Lifecycle,
    /// 2 — admit a queued `UserMessage` and start a turn (IntakeSystem).
    Intake,
    /// 3 — settle terminal results into History, regardless of gate (TurnSystem),
    /// and settle surface inputs into `Resources.surfaces` (SurfaceSystem).
    SettleResults,
    /// 4 — budget fold + compaction checks. P2.
    BudgetCompaction,
    /// 5 — turn-advance / continuation decisions (ToolSystem emits `RunTool` and
    /// the tool-loop continuation `CallModel`).
    TurnAdvance,
}

/// The fixed order phases 1–5 run in each tick. Phase 6 (Emit) follows in `tick`.
const PHASE_ORDER: [Phase; 5] = [
    Phase::Lifecycle,
    Phase::Intake,
    Phase::SettleResults,
    Phase::BudgetCompaction,
    Phase::TurnAdvance,
];

/// The registered Systems and the phase each runs in. The ORDER Systems execute
/// is determined solely by `PHASE_ORDER` (then registry order within a phase),
/// never by which System was added first — the determinism the tick discipline
/// requires. Each later milestone registers its Systems at the right phase.
const REGISTRY: &[(Phase, &'static dyn System)] = &[
    // GateSystem folds the pause/resume/clear inputs into gate state FIRST (phase 1),
    // so Intake (phase 2) and the turn-advance phase see the gate this tick decided.
    // It blocks NEW WORK only; result-settling (phase 3) never consults the gate
    // (Inv 13). See docs/agent/world/ecs-runtime.md (GateSystem; Tick discipline).
    (Phase::Lifecycle, &gate::GateSystem),
    // CancelSystem drives the `Cancel` transition and the `Cancelling` absorber in
    // phase 1, BEFORE any settling System: entering `Cancelling` it owes every
    // in-flight `cmd` and emits the abort Commands, and while `Cancelling` it absorbs
    // the FIRST terminal input for each owed `cmd` (abort ack or racing real result)
    // — so a later result for an already-absorbed `cmd` is dropped by the state-
    // guarded settling Systems, never double-processed. `Cancel` is total across every
    // active state (Inv 10, 14). See docs/agent/world/ecs-runtime.md (CancelSystem).
    (Phase::Lifecycle, &cancel::CancelSystem),
    // SteeringSystem owns the `UserMessage` half of steering (phase 2): a mid-run
    // message does NOT interrupt the in-flight turn (steering is a COMPOSITION —
    // `UserMessage` + optional `Cancel` — never its own Input). It runs BEFORE
    // IntakeSystem so it sees the PRE-admission `Activity`: if the entity is `Idle`
    // it no-ops (IntakeSystem, running next, will admit and start the turn); if the
    // entity is in any active state it enqueues the message FIFO. This ordering
    // removes the need for any content-equality heuristic and ensures every genuine
    // mid-run `UserMessage` — including repeated identical text — is durably parked.
    // See docs/agent/world/ecs-runtime.md (SteeringSystem; Tick discipline phase 2).
    (Phase::Intake, &steering::SteeringSystem),
    (Phase::Intake, &intake::IntakeSystem),
    (Phase::SettleResults, &turn::TurnSystem),
    // SurfaceSystem settles the surface inputs — a human `SurfaceMutated`, a
    // `SurfaceObserved` perception, and the `set_value`/UI `ToolReturned`
    // projection — into `Resources.surfaces` (phase 3). It touches only the
    // surface view, disjoint from TurnSystem's entity History/Activity, so their
    // order within the phase is immaterial. See docs/agent/world/ecs-runtime.md
    // (SurfaceSystem; Tick discipline phase 3).
    (Phase::SettleResults, &crate::agent::world::surface::SurfaceSystem),
    // BudgetSystem folds `ModelResponded.meta.usage` into the entity's per-entity
    // `Budget` (phase 4 BudgetCompaction): the cumulative SPEND account accumulates
    // and the single-request CONTEXT account is refreshed (two DISTINCT quantities).
    // On SPEND exhaustion it halts THIS entity's `EntityGate` (NOT the WorldGate, so
    // one entity's exhaustion never freezes the App) and settles its open slots
    // `is_error` so it cannot deadlock (Inv 10); the reused `ClearPolicyHalt` is the
    // named clearing input that reopens it (Inv 15). It runs after phase-3 settling
    // so an in-flight result is folded into the budget the same tick, and before the
    // phase-5 turn-advance so a halted entity's continuation is suppressed. See
    // docs/agent/world/ecs-runtime.md (BudgetSystem; Tick discipline phase 4).
    (Phase::BudgetCompaction, &budget::BudgetSystem),
    // CompactionSystem is the context-window guard (phase 4, after BudgetSystem so
    // the CONTEXT account is already folded when this runs). On `ModelResponded`
    // it detects context pressure (`Budget.context_exceeded()`) with a pending
    // continuation (non-empty Inbox), defers the continuation behind a `Compact`
    // model call (`Idle → Compacting`), and emits `Command::Compact`. On
    // `Compacted` it splices the summary into History, drains the Inbox, and
    // resumes the continuation (`→ Thinking` + `CallModel`). On a compaction
    // `ModelFailed` it proceeds UN-compacted (best-effort; a compaction failure is
    // never a new sink — Inv 10). See docs/agent/world/ecs-runtime.md
    // (CompactionSystem; Context-window compaction; Tick discipline phase 4).
    (Phase::BudgetCompaction, &compaction::CompactionSystem),
    // AutonomySystem gates `Local` slots requiring approval (phase 5, BEFORE
    // ToolSystem): on `ModelResponded` it sets gatable slots to `Pending{GATED}`
    // so ToolSystem skips them, emits `RaiseInteraction`, and records the pending
    // request in `Resources.raised`. On `InteractionAnswer` (before
    // InteractionSystem) it resets GATED slots to `Pending{cmd:None}` so
    // InteractionSystem can release them via its existing `raised`/release
    // mechanism. On `SetAutonomy` it updates `Resources.autonomy`. See
    // docs/agent/world/ecs-runtime.md (AutonomySystem; Tick discipline phase 5).
    (Phase::TurnAdvance, &autonomy::AutonomySystem),
    // ToolSystem drives a `ResolvingToolUses` turn's `Local` slots: it emits the
    // `RunTool`s and assembles the continuation `CallModel` — turn-advance work
    // (phase 5), which gates suppress when Closed. It runs strictly after
    // TurnSystem's phase-3 slot creation, and folds each `ToolReturned` into its
    // slot as part of the same step. See docs/agent/world/ecs-runtime.md (Tick
    // discipline phase 5; ToolSystem).
    (Phase::TurnAdvance, &tool::ToolSystem),
    // HumanActionSystem drives a `ResolvingToolUses` turn's `Human` slots (phase 5):
    // emits `RequestHumanAction` per pending `Human` slot, settles `HumanActionDone`
    // and `HumanActionAborted` into their slots, and shares the all-slots-Done
    // continuation gate with ToolSystem via `tool::advance`. Runs after ToolSystem so
    // Local slots already carry their minted cmds before the all-Done check. See
    // docs/agent/world/ecs-runtime.md (HumanActionSystem; Tick discipline phase 5).
    (Phase::TurnAdvance, &human_action::HumanActionSystem),
    // InteractionSystem correlates `RaiseInteraction` ↔ `InteractionAnswer` by
    // `request_id` (phase 5): `Accepted`/`Data` → release the held `Local` slot
    // (emit `RunTool`); `Rejected` → resolve with `is_error` (Inv 10, no deadlock).
    // Shares the all-slots-Done gate with ToolSystem via `tool::advance`. See
    // docs/agent/world/ecs-runtime.md (InteractionSystem; Tick discipline phase 5).
    (Phase::TurnAdvance, &interaction::InteractionSystem),
    // SubagentSystem owns the `Child` slot kind — in-process sub-agents (phase 5).
    // On the branching `ModelResponded` it spawns each child Entity (lineage = parent,
    // depth + 1, seeded with a `Thinking` turn + its `CallModel`) or DENIES a
    // depth-capped spawn inline with an `is_error` result (Inv 10, no deadlock); on
    // `ChildReturned` it settles the matching `Child` slot — correlated by
    // `(child, tool_use_id)`, NOT by `cmd` (Inv 16) — and shares the all-slots-Done
    // continuation via `tool::advance`. Runs after ToolSystem so a mixed turn's
    // `Local` slots already carry their cmds before its all-Done check. See
    // docs/agent/world/ecs-runtime.md (SubagentSystem; Tick discipline phase 5).
    (Phase::TurnAdvance, &subagent::SubagentSystem),
    // SupervisionSystem makes a dead in-process child non-blocking (phase 5): a child
    // Entity's terminal `ModelFailed` resolves the PARENT's owning `Child` slot
    // `is_error` so the parent never waits forever on a child that produced no result
    // (Inv 10), then shares the all-slots-Done continuation via `tool::advance`. It
    // touches only the parent's slot, disjoint from TurnSystem's child settle. See
    // docs/agent/world/ecs-runtime.md (SupervisionSystem; Tick discipline phase 5).
    (Phase::TurnAdvance, &supervision::SupervisionSystem),
    // PeerDriveSystem owns the `Peer` slot kind — cross-World drives (phase 5). On the
    // branching `ModelResponded` it emits each fresh `Peer` slot's `SendPeer` (or
    // DENIES a malformed `drive_peer` inline `is_error`, Inv 10); on the sender-local
    // `PeerSendOutcome` it settles the matching `Peer` slot by `cmd` (Inv 16) and
    // shares the all-slots-Done continuation via `tool::advance`. It ALSO folds an
    // inbound `DriveRequested` (origin Peer): a gate-proof exogenous settle that
    // authorizes the drive, projects its `surface_ops` (no separate `SurfaceMutated`),
    // and enqueues its `prompt` to the Inbox. Runs after ToolSystem so a mixed turn's
    // `Local` slots already carry their cmds before its all-Done check. See
    // docs/agent/world/ecs-runtime.md (PeerDriveSystem; Tick discipline phase 5).
    (Phase::TurnAdvance, &peer::PeerDriveSystem),
];

/// The pure tick reducer: process EXACTLY ONE `Event` through the registered
/// Systems in the fixed 6-phase order and return the next `World` with the tick's
/// emitted `Vec<Command>`.
///
/// Before any System runs it folds the Event's recorded wall-time into
/// `Resources.wall` (the wall recording boundary) and advances the logical
/// `clock` to the Event's tick — the only ambient facts a System may observe
/// enter the World HERE, never via a System reading the real clock (Inv 1/2). It
/// then threads the `World` through each phase's Systems, accumulating their
/// Commands. Phase 6 (Emit) is the final step: the accumulated `commands` list is
/// FIXED, and each effectful Command's intra-tick `effect_id` ordinal is its
/// position in THIS list (Inv 12) — so ordinals never depend on which System
/// emitted first. P0's `CommandKey` is a placeholder, so there is nothing further
/// to stamp; the WAL milestone (P2) stamps the full `IdempotencyKey` here.
pub fn tick(world: &World, event: &Event) -> (World, Vec<Command>) {
    let mut world = world.clone();
    // Fold the recorded wall-time (hidden-input boundary) before Systems run.
    if let Some(wall) = event.wall {
        world.resources.wall.observed = Some(wall);
    }
    // Advance the logical ordering counter to this Event's tick.
    world.clock = event.at;

    let input = &event.input;

    // Structural counterpart→edge binding: an `EdgeBound` records the inbound peer's
    // edge into `Resources.edges` BEFORE the phase Systems run, so a re-resolution
    // (`edge_for`) is stable across that peer's inputs and a reconstruction folds the
    // SAME binding (Inv 6). It belongs to no phase — a structural fold like the
    // wall/clock fold above — and is folded identically by the LIVE driver and a
    // replay, so both produce byte-identical `Resources`. A no-op for every
    // pre-federation log (only a fresh `Peer` ever logs one). See
    // docs/agent/world/ecs-runtime.md (EdgeBound; edge_for; Theme 4a).
    if let LogicalInput::EdgeBound { edge, counterpart } = input {
        world.resources =
            crate::agent::world::edge::fold_edge_bound(&world.resources, *edge, counterpart);
    }

    let mut commands: Vec<Command> = Vec::new();

    // Phases 1–5: run each phase's Systems in the fixed phase order (Inv 12).
    for phase in PHASE_ORDER {
        for (registered, system) in REGISTRY {
            if *registered == phase {
                let (next, mut cmds) = system.step(&world, input);
                world = next;
                commands.append(&mut cmds);
            }
        }
    }

    // Phase 6 — Emit: `commands` is the final, fixed list; ordinals derive from it.
    (world, commands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::effects::Command;
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{
        CancelReason, Capabilities, Event, Fingerprint, LogicalInput, ModelError, ModelMeta, Origin,
        ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, Tick, World,
    };

    /// A World with a single primary entity at `Idle`, ready to take a turn.
    fn idle_world() -> World {
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        };
        let mut world = World::new(0, Resources::new(42, model));
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
                budget: crate::agent::world::budget::Budget::default(),
                inbox: crate::agent::world::world::Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
            },
        );
        world
    }

    fn user_message(text: &str, at: Tick) -> Event {
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

    fn model_responded(cmd: CmdId, text: &str, stop: StopReason, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::Text { text: text.into() }],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: "claude-x".into(),
                    stop_reason: stop,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    fn model_failed(cmd: CmdId, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelFailed {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                error: ModelError::Http(503),
            },
        }
    }

    fn inference_cancelled(cmd: CmdId, reason: CancelReason, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::InferenceCancelled {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                partial: None,
                reason,
            },
        }
    }

    fn call_model_cmd(command: &Command) -> CmdId {
        match command {
            Command::CallModel { cmd, .. } => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    #[test]
    fn intake_idle_to_thinking_emits_one_call_model() {
        let world = idle_world();
        let (world1, commands) = tick(&world, &user_message("hello", 1));

        // Intake emits EXACTLY one CallModel, routed to the entity, carrying the
        // just-appended user message in its request History.
        assert_eq!(commands.len(), 1, "intake must emit exactly one CallModel");
        let cmd = match &commands[0] {
            Command::CallModel {
                cmd,
                entity,
                messages,
                ..
            } => {
                assert_eq!(*entity, 0);
                assert_eq!(messages.0.len(), 1, "the user message rides in the request");
                assert!(matches!(
                    &messages.0[0],
                    Msg { role: Role::User, content }
                        if matches!(content.as_slice(), [Block::Text { text }] if text == "hello")
                ));
                *cmd
            }
            other => panic!("expected CallModel, got {other:?}"),
        };

        // Idle → Thinking on the minted cmd, with the user message in History.
        let e = world1.entities.get(&0).expect("entity present");
        assert!(
            matches!(e.activity, Activity::Thinking { cmd: c } if c == cmd),
            "Idle→Thinking on the minted cmd"
        );
        assert_eq!(e.history.0.len(), 1);
    }

    #[test]
    fn model_responded_end_turn_returns_to_idle_with_assistant_appended() {
        let world = idle_world();
        let (world1, commands) = tick(&world, &user_message("hi", 1));
        let cmd = call_model_cmd(&commands[0]);

        let (world2, commands2) =
            tick(&world1, &model_responded(cmd, "the answer", StopReason::EndTurn, 2));

        // EndTurn (text path) emits no continuation Command and returns to Idle,
        // with the assistant blocks appended to History.
        assert!(commands2.is_empty(), "EndTurn emits no continuation Command");
        let e = world2.entities.get(&0).expect("entity present");
        assert!(matches!(e.activity, Activity::Idle), "EndTurn returns Thinking→Idle");
        assert_eq!(e.history.0.len(), 2, "user + assistant");
        assert!(matches!(
            &e.history.0[1],
            Msg { role: Role::Assistant, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == "the answer")
        ));
    }

    #[test]
    fn model_failed_returns_thinking_to_idle_not_a_sink() {
        let world = idle_world();
        let (world1, commands) = tick(&world, &user_message("hi", 1));
        let cmd = call_model_cmd(&commands[0]);
        assert!(matches!(
            world1.entities.get(&0).expect("entity present").activity,
            Activity::Thinking { .. }
        ));

        let (world2, commands2) = tick(&world1, &model_failed(cmd, 2));

        // ModelFailed gives Thinking a terminating edge → Idle (Inv 10), no Command.
        assert!(commands2.is_empty());
        assert!(
            matches!(
                world2.entities.get(&0).expect("entity present").activity,
                Activity::Idle
            ),
            "ModelFailed must not leave Thinking a sink (Inv 10)"
        );
    }

    #[test]
    fn inference_cancelled_crash_returns_thinking_to_idle_not_a_sink() {
        let world = idle_world();
        let (world1, commands) = tick(&world, &user_message("hi", 1));
        let cmd = call_model_cmd(&commands[0]);
        assert!(matches!(
            world1.entities.get(&0).expect("entity present").activity,
            Activity::Thinking { .. }
        ));

        // A crash synthesises a stratum-1 InferenceCancelled { Crash } (effects.rs
        // resume); folding it gives Thinking a terminating edge → Idle (Inv 10).
        let (world2, commands2) =
            tick(&world1, &inference_cancelled(cmd, CancelReason::Crash, 2));

        assert!(commands2.is_empty(), "a crash cancellation emits no continuation");
        assert!(
            matches!(
                world2.entities.get(&0).expect("entity present").activity,
                Activity::Idle
            ),
            "InferenceCancelled{{Crash}} must not leave Thinking a sink (Inv 10)"
        );
    }
}
