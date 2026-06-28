//! cancel — the CancelSystem (phase 1 Lifecycle). See docs/agent/world/ecs-runtime.md
//! (CancelSystem; `Cancelling` absorber; Invariants 10 and 14).
//!
//! `Cancel` is a TOTAL input — defined in EVERY active state (Totality, Inv 10), not
//! only `Thinking`. CancelSystem owns two roles, both pure folds of `(World, Input)`:
//!
//! - **Cancel transition** — on `Cancel { entity }` in an active state it collects
//!   EVERY owed ack (the in-flight `cmd`s), transitions the entity to
//!   `Activity::Cancelling { awaiting }`, and emits the matching abort Commands:
//!   `Thinking`/`Compacting` owe their `cmd` (emit `CancelInference`); each
//!   `ResolvingToolUses` slot still `Pending { cmd: Some(c) }` owes `c` (emit
//!   `CancelTool` for `Local`, `AbortHumanAction` for `Human`; a `Peer` slot's
//!   durable send is NOT recallable, so it is merely awaited). `Idle`/`Cancelling`
//!   → idempotent no-op (nothing in flight). A turn with no owed ack collapses
//!   straight to `Idle`.
//! - **Cancelling absorber** — while `Cancelling`, the FIRST terminal input for an
//!   awaited `cmd` — the abort ack (`InferenceCancelled`/`ToolAborted`/
//!   `HumanActionAborted`) OR a real result that races the abort (`ModelResponded`/
//!   `ModelFailed`/`ToolReturned`/`HumanActionDone`/`Compacted`) — is ABSORBED:
//!   that `cmd` is removed from `awaiting` and its result is DISCARDED (never folded
//!   into History as a live turn result). When `awaiting` is empty `→ Idle` (Inv 14).
//!
//! Arbitration is keyed on `(cmd, state)`: because CancelSystem runs FIRST (phase 1),
//! it absorbs an awaited result before TurnSystem (phase 3) or ToolSystem (phase 5)
//! could settle it — and those Systems guard on the specific active state
//! (`Thinking`-on-`cmd` / `ResolvingToolUses`-slot-`cmd`), which a `Cancelling`
//! entity is not, so a later terminal input for an already-absorbed `cmd` is dropped,
//! never double-processed (Inv 10). Steering is modelled separately as a COMPOSITION
//! (`UserMessage` + optional `Cancel`); see steering.rs.

use super::{Input, System};
use crate::agent::world::effects::Command;
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, CmdId, EntityId, SlotKind, SlotState, World, HELD_CMD};

/// CancelSystem — drives the `Cancel` transition and the `Cancelling` absorber
/// (phase 1 of the tick, so it absorbs an owed ack before any settling System).
pub struct CancelSystem;

impl System for CancelSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // Cancel: enter `Cancelling` from any active state, owing every in-flight
            // `cmd`, and emit the abort Commands; `Idle`/`Cancelling` → no-op (Inv 14).
            LogicalInput::Cancel { entity } => cancel(world, entity),
            // Absorber: a terminal input for an awaited `cmd` while `Cancelling` clears
            // that `cmd` (its result discarded); `→ Idle` when `awaiting` empties. A
            // strict no-op unless the entity is `Cancelling` and owes this `cmd`, so a
            // normal result reaches its settling System (TurnSystem/ToolSystem) intact.
            LogicalInput::ToolReturned { cmd, entity, .. }
            | LogicalInput::ToolAborted { cmd, entity }
            | LogicalInput::HumanActionAborted { cmd, entity }
            | LogicalInput::HumanActionDone { cmd, entity, .. }
            | LogicalInput::ModelResponded { cmd, entity, .. }
            | LogicalInput::ModelFailed { cmd, entity, .. }
            | LogicalInput::InferenceCancelled { cmd, entity, .. }
            | LogicalInput::Compacted { cmd, entity, .. } => absorb(world, entity, *cmd),
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// The `Cancel` transition: read the entity's active state to collect every owed
/// `cmd` and its abort Command, then move the entity to `Cancelling { awaiting }`
/// (or straight to `Idle` when nothing is owed). `Idle`/`Cancelling`/absent →
/// idempotent no-op. See docs/agent/world/ecs-runtime.md (CancelSystem; Inv 14).
///
/// Held-for-approval slots (`Pending { cmd: Some(HELD_CMD) }`) have NO effect in
/// flight, so they never enter `awaiting` and receive no abort Command. Their
/// `resources.raised` entry is cleared so the stale approval request cannot fire.
/// This makes Cancel total over HELD_CMD slots (Inv 10/14 — no infinite await).
fn cancel(world: &World, entity: &EntityId) -> (World, Vec<Command>) {
    // Read phase: derive the owed acks and abort Commands from the current activity,
    // releasing the borrow before the mutation below (a pure function of the state).
    // `held` collects the `tool_use_id`s of HELD_CMD (held-for-approval) slots whose
    // `resources.raised` entries must be cleared in the write phase.
    let plan: Option<(Vec<CmdId>, Vec<Command>, Vec<String>)> =
        match world.entities.get(entity).map(|e| &e.activity) {
            // A model/compaction call in flight owes exactly its `cmd`.
            Some(Activity::Thinking { cmd }) | Some(Activity::Compacting { cmd }) => {
                Some((vec![*cmd], vec![Command::CancelInference { cmd: *cmd }], vec![]))
            }
            // Every still-`Pending` slot owes its `cmd`; the abort Command depends on
            // the slot kind. A `Done` slot keeps its result and owes nothing; a `Child`
            // slot (`Pending { cmd: None }`) is identity-correlated and resolved by
            // SubagentSystem (out of scope here), so it carries no owed `cmd`.
            // A HELD_CMD slot is held-for-approval (no effect dispatched): it is NOT
            // added to `awaiting` and receives no abort Command; instead its `raised`
            // entry is removed in the write phase (Inv 10/14 — no infinite await).
            Some(Activity::ResolvingToolUses { slots }) => {
                let mut awaiting = Vec::new();
                let mut commands = Vec::new();
                let mut held: Vec<String> = Vec::new();
                for slot in slots {
                    if let SlotState::Pending { cmd: Some(c) } = slot.state {
                        if c == HELD_CMD {
                            // Held-for-approval: no effect in flight; rescind the
                            // pending approval request in the write phase.
                            held.push(slot.tool_use_id.clone());
                        } else {
                            awaiting.push(c);
                            match slot.kind {
                                SlotKind::Local => commands.push(Command::CancelTool { cmd: c }),
                                SlotKind::Human => {
                                    commands.push(Command::AbortHumanAction { cmd: c })
                                }
                                // A peer drive's durable send sits in the at-least-once
                                // outbox and is not recallable: no abort is emitted, but
                                // its owed `PeerSendOutcome` is recorded and absorbed on
                                // arrival.
                                SlotKind::Peer => {}
                                // Unreachable for `Pending { cmd: Some(_) }` (a `Child`
                                // slot is `cmd: None`); listed for totality.
                                SlotKind::Child(_) => {}
                            }
                        }
                    }
                }
                Some((awaiting, commands, held))
            }
            // `Idle`/`Cancelling`/absent: nothing in flight → idempotent no-op.
            _ => None,
        };

    let Some((awaiting, commands, held)) = plan else {
        return (world.clone(), Vec::new());
    };

    // Write phase: apply the transition. An empty `awaiting` (nothing owed) collapses
    // straight to `Idle` rather than parking in an already-satisfied `Cancelling`.
    let mut world = world.clone();

    // Clear `resources.raised` entries for held-for-approval slots. These slots had no
    // effect in flight; removing their raised entries ensures the stale approval can
    // never fire and gives Cancel totality over HELD_CMD slots (Inv 10/14).
    if !held.is_empty() {
        world
            .resources
            .raised
            .retain(|_, (eid, tuid)| !(*eid == *entity && held.contains(tuid)));
    }

    if let Some(e) = world.entities.get_mut(entity) {
        e.activity = if awaiting.is_empty() {
            Activity::Idle
        } else {
            Activity::Cancelling { awaiting }
        };
    }
    (world, commands)
}

/// The `Cancelling` absorber: if `entity` is `Cancelling` and owes `cmd`, remove it
/// from `awaiting` (discarding the racing result) and return to `Idle` once nothing
/// remains owed. A no-op otherwise — so a result for a non-`Cancelling` entity, or a
/// late result for an already-absorbed `cmd`, is left for (or dropped by) the normal
/// settling Systems, never double-processed (Inv 10/14).
fn absorb(world: &World, entity: &EntityId, cmd: CmdId) -> (World, Vec<Command>) {
    let mut world = world.clone();
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::Cancelling { awaiting } = &mut e.activity
        && let Some(pos) = awaiting.iter().position(|c| *c == cmd)
    {
        awaiting.remove(pos);
        if awaiting.is_empty() {
            e.activity = Activity::Idle;
        }
    }
    // The absorber emits no Commands: the abort was already dispatched on the `Cancel`
    // transition; a racing real result needs no further effect.
    (world, Vec::new())
}

#[cfg(test)]
mod tests {
    use crate::agent::world::budget::Budget;
    use crate::agent::world::effects::Command;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{
        CancelReason, Event, Fingerprint, LogicalInput, ModelError, Origin,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, SlotKind,
        SlotState, Tick, ToolSlot, World, HELD_CMD,
    };

    /// A World whose single primary entity sits in the given `Activity`, with an
    /// optional pre-seeded `History` so a "no History fold" assertion is meaningful.
    fn world_in(activity: Activity, history: History) -> World {
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
                history,
                activity,
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: crate::agent::world::world::Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );
        world
    }

    fn cancel_event(at: Tick) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::Cancel { entity: 0 },
        }
    }

    fn tool_aborted_event(cmd: CmdId, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ToolAborted { cmd, entity: 0 },
        }
    }

    fn tool_returned_event(cmd: CmdId, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ToolReturned {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                result: vec![Block::Text {
                    text: "late".into(),
                }],
            },
        }
    }

    fn inference_cancelled_event(cmd: CmdId, at: Tick) -> Event {
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
                reason: CancelReason::Requested,
            },
        }
    }

    fn model_failed_event(cmd: CmdId, at: Tick) -> Event {
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

    fn activity_of(world: &World) -> Activity {
        world.entities.get(&0).expect("entity").activity.clone()
    }

    fn two_local_pending_slots() -> Vec<ToolSlot> {
        vec![
            ToolSlot {
                tool_use_id: "tu0".into(),
                ordinal: 0,
                kind: SlotKind::Local,
                state: SlotState::Pending { cmd: Some(0) },
                result: None,
            },
            ToolSlot {
                tool_use_id: "tu1".into(),
                ordinal: 1,
                kind: SlotKind::Local,
                state: SlotState::Pending { cmd: Some(1) },
                result: None,
            },
        ]
    }

    /// VC-1.3: `Cancel` during `ResolvingToolUses` with two `Pending` `Local` slots
    /// owes both `cmd`s → `Cancelling { awaiting: [0, 1] }` and emits exactly two
    /// `CancelTool` aborts; feeding the two `ToolAborted` acks absorbs them one by
    /// one and reaches `Idle` (Inv 14). The whole flow runs through the registered
    /// tick pipeline, so it also proves CancelSystem's phase placement and that no
    /// sibling System double-handles the absorbed acks.
    #[test]
    fn cancel_resolving_tool_uses_absorbs_both_aborts_to_idle() {
        let world = world_in(
            Activity::ResolvingToolUses {
                slots: two_local_pending_slots(),
            },
            History::default(),
        );

        // Cancel → Cancelling owing both cmds, with one CancelTool per Pending slot.
        let (world, commands) = tick(&world, &cancel_event(1));
        assert_eq!(
            activity_of(&world),
            Activity::Cancelling {
                awaiting: vec![0, 1]
            },
            "Cancel owes every Pending slot's cmd"
        );
        assert_eq!(
            commands,
            vec![
                Command::CancelTool { cmd: 0 },
                Command::CancelTool { cmd: 1 }
            ],
            "one CancelTool abort per Pending Local slot"
        );

        // First ack absorbs cmd 0; still owes cmd 1.
        let (world, commands) = tick(&world, &tool_aborted_event(0, 2));
        assert!(commands.is_empty(), "an absorbed ack emits no Command");
        assert_eq!(
            activity_of(&world),
            Activity::Cancelling { awaiting: vec![1] },
            "the first ToolAborted clears its cmd; one still owed"
        );

        // Second ack empties `awaiting` → Idle.
        let (world, commands) = tick(&world, &tool_aborted_event(1, 3));
        assert!(commands.is_empty());
        assert_eq!(
            activity_of(&world),
            Activity::Idle,
            "the last owed ack absorbed → Idle (Inv 14)"
        );
    }

    /// VC-1.3 (arbitration, Inv 10): a late `ToolReturned` for a `cmd` already
    /// absorbed (the entity is back to `Idle`) is DROPPED — neither CancelSystem nor
    /// ToolSystem folds its result into History; the entity stays `Idle`.
    #[test]
    fn late_result_for_absorbed_cmd_is_dropped() {
        // An entity that has already cancelled and settled to Idle, with a non-empty
        // History so a stray fold would be observable as a length change.
        let history = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text {
                text: "hi".into(),
            }],
        }]);
        let world = world_in(Activity::Idle, history);
        let history_len_before = world.entities.get(&0).expect("entity").history.0.len();

        // A late tool result for the already-absorbed cmd 0 arrives.
        let (world, commands) = tick(&world, &tool_returned_event(0, 9));

        assert!(
            commands.is_empty(),
            "a late result for an absorbed cmd drives no continuation"
        );
        assert_eq!(
            activity_of(&world),
            Activity::Idle,
            "the entity stays Idle — the late result is dropped"
        );
        assert_eq!(
            world.entities.get(&0).expect("entity").history.0.len(),
            history_len_before,
            "no History fold for the late, already-absorbed result (Inv 10)"
        );
    }

    /// VC-1.3: `Cancel` during `Thinking` owes the model `cmd` → `Cancelling`
    /// (emit `CancelInference`); absorbing the `InferenceCancelled` ack → `Idle`.
    #[test]
    fn cancel_thinking_then_absorb_inference_cancelled_to_idle() {
        let world = world_in(Activity::Thinking { cmd: 5 }, History::default());

        let (world, commands) = tick(&world, &cancel_event(1));
        assert_eq!(
            activity_of(&world),
            Activity::Cancelling { awaiting: vec![5] }
        );
        assert_eq!(
            commands,
            vec![Command::CancelInference { cmd: 5 }],
            "Thinking owes its model cmd → CancelInference"
        );

        // The abort ack races back and is absorbed → Idle (no History fold).
        let (world, commands) = tick(&world, &inference_cancelled_event(5, 2));
        assert!(commands.is_empty());
        assert_eq!(
            activity_of(&world),
            Activity::Idle,
            "absorbing the owed InferenceCancelled empties awaiting → Idle"
        );
    }

    /// Inv 14: a REAL result (here `ModelFailed`) racing the abort while `Cancelling`
    /// is absorbed as the owed ack — it settles the cancel, it does not re-open a turn.
    #[test]
    fn racing_real_result_is_absorbed_not_settled_as_a_turn() {
        let world = world_in(Activity::Thinking { cmd: 7 }, History::default());
        let (world, _) = tick(&world, &cancel_event(1));
        assert_eq!(
            activity_of(&world),
            Activity::Cancelling { awaiting: vec![7] }
        );

        // The model call actually FAILED at the same time the cancel was requested:
        // the failure is absorbed as cmd 7's owed terminal input → Idle, not folded.
        let (world, commands) = tick(&world, &model_failed_event(7, 2));
        assert!(commands.is_empty(), "an absorbed racing result drives no turn");
        assert_eq!(
            activity_of(&world),
            Activity::Idle,
            "the racing ModelFailed is absorbed as the owed ack (Inv 14)"
        );
    }

    /// VC-1.6 + Inv 10/14 (HELD-sentinel fix): `Cancel` on an entity whose only
    /// `ResolvingToolUses` slot is held-for-approval (`Pending { cmd: Some(HELD_CMD) }`,
    /// recorded in `resources.raised`) must NOT add `HELD_CMD` to `awaiting`, must NOT
    /// emit any abort Command, must clear the `raised` entry (so the stale approval
    /// cannot fire), and must reach `Idle` immediately — never stuck in `Cancelling`
    /// waiting on an ack that will never arrive (no deadlock, Inv 10/14).
    #[test]
    fn cancel_held_slot_resolves_is_error_clears_raised_no_abort() {
        let tool_use_id = "tu_held";
        let request_id: u32 = 99;

        // Build a world with entity 0 in ResolvingToolUses with a single HELD_CMD slot
        // (a Local slot held-for-approval by AutonomySystem) and its raised entry.
        let mut world = world_in(
            Activity::ResolvingToolUses {
                slots: vec![ToolSlot {
                    tool_use_id: tool_use_id.into(),
                    ordinal: 0,
                    kind: SlotKind::Local,
                    state: SlotState::Pending { cmd: Some(HELD_CMD) },
                    result: None,
                }],
            },
            History::default(),
        );
        world
            .resources
            .raised
            .insert(request_id, (0, tool_use_id.into()));

        let (world, commands) = tick(&world, &cancel_event(1));

        // No abort commands must be emitted — HELD_CMD has no effect in flight.
        assert!(
            commands.is_empty(),
            "Cancel on a HELD slot must emit no abort commands (no effect in flight)"
        );
        // Entity must reach Idle — not stuck in Cancelling awaiting HELD_CMD (Inv 14).
        assert_eq!(
            activity_of(&world),
            Activity::Idle,
            "Cancel on a HELD-only entity must reach Idle immediately (no infinite await, Inv 10/14)"
        );
        // HELD_CMD must NOT appear in any awaiting list (entity is Idle, trivially true).
        // More importantly, the stale raised entry must be cleared so the approval cannot fire.
        assert!(
            !world.resources.raised.contains_key(&request_id),
            "Cancel must clear the resources.raised entry for the HELD slot"
        );
    }

    /// VC-1.6 + Inv 10/14 (mixed slots): `Cancel` on an entity with both a real
    /// pending slot and a HELD_CMD slot enters `Cancelling` for the real slot only —
    /// HELD_CMD is absent from `awaiting`, no abort is emitted for it, and its
    /// `raised` entry is cleared.
    #[test]
    fn cancel_mixed_held_and_real_slots_held_not_in_awaiting() {
        let tool_use_id_held = "tu_held";
        let tool_use_id_real = "tu_real";
        let request_id: u32 = 77;
        let real_cmd: CmdId = 5;

        let mut world = world_in(
            Activity::ResolvingToolUses {
                slots: vec![
                    ToolSlot {
                        tool_use_id: tool_use_id_held.into(),
                        ordinal: 0,
                        kind: SlotKind::Local,
                        state: SlotState::Pending { cmd: Some(HELD_CMD) },
                        result: None,
                    },
                    ToolSlot {
                        tool_use_id: tool_use_id_real.into(),
                        ordinal: 1,
                        kind: SlotKind::Local,
                        state: SlotState::Pending { cmd: Some(real_cmd) },
                        result: None,
                    },
                ],
            },
            History::default(),
        );
        world
            .resources
            .raised
            .insert(request_id, (0, tool_use_id_held.into()));

        let (world, commands) = tick(&world, &cancel_event(1));

        // Only the real slot's abort is emitted; the HELD slot gets none.
        assert_eq!(
            commands,
            vec![Command::CancelTool { cmd: real_cmd }],
            "only the real slot's CancelTool is emitted; HELD slot gets no abort"
        );
        // Entity is Cancelling awaiting only the real cmd.
        assert_eq!(
            activity_of(&world),
            Activity::Cancelling { awaiting: vec![real_cmd] },
            "HELD_CMD must not appear in awaiting; only the real cmd is owed"
        );
        // The HELD slot's raised entry is cleared.
        assert!(
            !world.resources.raised.contains_key(&request_id),
            "raised entry for the HELD slot must be removed on Cancel"
        );
    }

    /// `Cancel` while `Idle` is an idempotent no-op (nothing is in flight), and a
    /// second `Cancel` while already `Cancelling` does not disturb `awaiting`.
    #[test]
    fn cancel_is_idempotent_in_idle_and_cancelling() {
        // Idle → no-op.
        let world = world_in(Activity::Idle, History::default());
        let (world, commands) = tick(&world, &cancel_event(1));
        assert!(commands.is_empty());
        assert_eq!(activity_of(&world), Activity::Idle, "Cancel in Idle is a no-op");

        // Cancelling → a second Cancel leaves `awaiting` untouched.
        let world = world_in(
            Activity::Cancelling {
                awaiting: vec![3, 4],
            },
            History::default(),
        );
        let (world, commands) = tick(&world, &cancel_event(2));
        assert!(commands.is_empty(), "a Cancel while Cancelling emits nothing");
        assert_eq!(
            activity_of(&world),
            Activity::Cancelling {
                awaiting: vec![3, 4]
            },
            "a re-Cancel does not disturb the owed acks"
        );
    }
}
