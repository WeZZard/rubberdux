//! budget — the BudgetSystem (phase 4 BudgetCompaction). See
//! docs/agent/world/ecs-runtime.md (BudgetSystem; Two budgets: spend vs context).
//!
//! BudgetSystem owns the per-entity SPEND brake. On every `ModelResponded` it
//! folds the call's `meta.usage` into the entity's `Budget` — ACCUMULATING the
//! cumulative `spend` account and REFRESHING the single-request `context` account
//! (two DISTINCT quantities, never conflated). When the cumulative SPEND crosses
//! its ceiling it HALTS this entity: it sets the per-entity `EntityGate.halt`
//! (NOT the App-wide WorldGate, so one entity exhausting its budget never freezes
//! the World) and settles every still-open `ToolSlot` with an `is_error`
//! `ToolResult` so the entity SETTLES rather than deadlocking (Invariant 10). The
//! halt's NAMED clearing input is the gate task's `ClearPolicyHalt { trip, .. }`
//! carrying the budget trip (Invariant 15), folded by GateSystem — no
//! near-duplicate clearing input is added here.
//!
//! The CONTEXT account is folded too, but a context overflow is the compaction
//! guard's concern (P2-compaction); only the SPEND limit drives the halt here.

use super::{Input, System};
use crate::agent::world::budget::budget_exhausted_trip;
use crate::agent::world::effects::Command;
use crate::agent::world::gates::EntityHalt;
use crate::agent::world::history::Block;
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, Components, SlotState, World};

/// The `is_error` payload a budget halt injects into each open slot, so the model
/// sees a well-formed `tool_result` explaining why the slot was force-settled.
const BUDGET_HALT_NOTE: &str = "spend budget exhausted; entity halted";

/// BudgetSystem — folds `ModelResponded.meta.usage` into the entity's `Budget`
/// (phase 4) and, on SPEND exhaustion, halts the per-entity `EntityGate` and
/// settles its open slots `is_error`. Pure reducer; emits no Commands.
pub struct BudgetSystem;

impl System for BudgetSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        // Only an inference result carries usage to fold; everything else is
        // orthogonal to the budget (handled by other phases).
        let LogicalInput::ModelResponded { entity, meta, .. } = input else {
            return (world.clone(), Vec::new());
        };
        let mut world = world.clone();
        if let Some(components) = world.entities.get_mut(entity) {
            // Fold the call's usage into the two DISTINCT accounts (spend
            // accumulates; context is refreshed).
            components.budget = components.budget.fold_usage(meta.usage);
            // SPEND exhaustion HALTS this entity. Only the FIRST crossing sets the
            // halt and settles slots — a standing halt (this trip or another) is
            // left untouched, so the fold stays idempotent on replay.
            if components.budget.spend_exhausted() && components.gate.halt.is_none() {
                components.gate.halt = Some(EntityHalt {
                    trip: budget_exhausted_trip(),
                });
                settle_open_slots_is_error(components);
            }
        }
        (world, Vec::new())
    }
}

/// Settle every still-`Pending` `ToolSlot` of a `ResolvingToolUses` entity with an
/// `is_error` `ToolResult`, so a budget halt never leaves the entity deadlocked on
/// an unresolved slot (Invariant 10 — settle, don't sink). Entities that are not
/// mid-resolution have no open slots and are left unchanged.
fn settle_open_slots_is_error(components: &mut Components) {
    if let Activity::ResolvingToolUses { slots } = &mut components.activity {
        for slot in slots.iter_mut() {
            if matches!(slot.state, SlotState::Pending { .. }) {
                slot.result = Some(Block::ToolResult {
                    tool_use_id: slot.tool_use_id.clone(),
                    content: vec![Block::Text {
                        text: BUDGET_HALT_NOTE.into(),
                    }],
                    is_error: true,
                });
                slot.state = SlotState::Done;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::budget::{Budget, Limits};
    use crate::agent::world::gates::{Authority, EntityGate, GuardrailTrip};
    use crate::agent::world::history::History;
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        CmdId, Effort, Identity, Lineage, ModelConfig, Resources, SlotKind, Tick, ToolSlot,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// A World with one primary entity carrying the given `activity` and `budget`.
    fn world_with(activity: Activity, budget: Budget) -> World {
        let mut world = World::new(0, Resources::new(42, model()));
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage {
                    parent: None,
                    depth: 0,
                },
                history: History::default(),
                activity,
                gate: EntityGate::default(),
                budget,
                inbox: crate::agent::world::world::Inbox::default(),
                turns: 0,
                model: None,
            },
        );
        world
    }

    fn model_responded(cmd: CmdId, usage: Usage, stop: StopReason, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::Text { text: "ok".into() }],
                meta: ModelMeta {
                    usage,
                    model_id: "claude-x".into(),
                    stop_reason: stop,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    fn clear_policy_halt(trip: GuardrailTrip, at: Tick) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ClearPolicyHalt {
                trip,
                authority: Authority("admin".into()),
            },
        }
    }

    /// VC-2.3 (core): folding a `ModelResponded` whose usage crosses `spend_limit`
    /// HALTS the per-entity `EntityGate` (NOT the WorldGate) and settles the
    /// entity's open slots `is_error` so it does not deadlock (Inv 10).
    #[test]
    fn spend_crossing_limit_halts_entity_gate_and_settles_open_slots() {
        // Mid-resolution with one open Local slot and a spend budget this response
        // will push over its ceiling (60 + 50 = 110 >= 100).
        let open_slot = ToolSlot {
            tool_use_id: "tu_0".into(),
            ordinal: 0,
            kind: SlotKind::Local,
            state: SlotState::Pending { cmd: Some(7) },
            result: None,
        };
        let budget = Budget {
            limits: Limits {
                spend_limit: 100,
                context_limit: 0,
                ..Default::default()
            },
            ..Budget::default()
        };
        let world = world_with(
            Activity::ResolvingToolUses {
                slots: vec![open_slot],
            },
            budget,
        );
        assert!(world.resources.gate.is_open(), "WorldGate starts open");
        assert!(
            world.entities.get(&0).expect("entity").gate.is_open(),
            "EntityGate starts open"
        );

        let usage = Usage {
            input_tokens: 60,
            output_tokens: 50,
        };
        let (world, commands) = tick(&world, &model_responded(0, usage, StopReason::ToolUse, 1));
        assert!(commands.is_empty(), "BudgetSystem emits no Commands");

        let e = world.entities.get(&0).expect("entity");
        assert_eq!(e.budget.spend.used, 110, "cumulative spend folded");
        assert_eq!(
            e.gate.halt,
            Some(EntityHalt {
                trip: budget_exhausted_trip()
            }),
            "spend exhaustion sets the per-entity EntityHalt"
        );
        assert!(!e.gate.is_open(), "the entity gate is halted");

        // The WorldGate is untouched — one entity halting never freezes the App.
        assert!(
            world.resources.gate.is_open(),
            "the WorldGate stays OPEN (per-entity halt, not the world)"
        );

        // The open slot was settled `is_error` (Inv 10 — settle, not deadlock).
        match &e.activity {
            Activity::ResolvingToolUses { slots } => {
                assert_eq!(slots.len(), 1);
                assert_eq!(slots[0].state, SlotState::Done, "open slot settled Done");
                match &slots[0].result {
                    Some(Block::ToolResult {
                        is_error,
                        tool_use_id,
                        ..
                    }) => {
                        assert!(*is_error, "settled slot carries an is_error ToolResult");
                        assert_eq!(tool_use_id, "tu_0");
                    }
                    other => panic!("expected an is_error ToolResult, got {other:?}"),
                }
            }
            other => panic!("entity should still carry its settled slots, got {other:?}"),
        }
    }

    /// VC-2.3 (clearing): the NAMED clearing input (reused `ClearPolicyHalt`)
    /// carrying the budget trip clears the `EntityHalt` and reopens the gate
    /// (Inv 15), folded by GateSystem.
    #[test]
    fn clear_policy_halt_reopens_the_budget_halted_entity() {
        let budget = Budget {
            limits: Limits {
                spend_limit: 50,
                context_limit: 0,
                ..Default::default()
            },
            ..Budget::default()
        };
        let world = world_with(Activity::Idle, budget);
        let usage = Usage {
            input_tokens: 40,
            output_tokens: 20,
        };
        let (world, _) = tick(&world, &model_responded(0, usage, StopReason::EndTurn, 1));
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(
            e.gate.halt,
            Some(EntityHalt {
                trip: budget_exhausted_trip()
            }),
            "the entity is budget-halted"
        );
        assert!(!e.gate.is_open());

        // ClearPolicyHalt carrying the matching budget trip reopens the gate.
        let (world, commands) = tick(&world, &clear_policy_halt(budget_exhausted_trip(), 2));
        assert!(commands.is_empty());
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(e.gate.halt, None, "ClearPolicyHalt cleared the budget halt");
        assert!(e.gate.is_open(), "the entity gate reopened");
        assert_eq!(
            e.budget.spend.used, 60,
            "clearing the halt does not reset the spend accounting"
        );
    }

    /// VC-2.3 (distinctness): a context-only overflow does NOT trip the SPEND
    /// halt — the spend and context budgets are distinct quantities.
    #[test]
    fn context_overflow_alone_does_not_trip_the_spend_halt() {
        // spend_limit high, context_limit low: this response pushes context over
        // its ceiling but keeps cumulative spend well under the spend ceiling.
        let budget = Budget {
            limits: Limits {
                spend_limit: 10_000,
                context_limit: 50,
                ..Default::default()
            },
            ..Budget::default()
        };
        let world = world_with(Activity::Idle, budget);
        let usage = Usage {
            input_tokens: 70,
            output_tokens: 30,
        }; // 100 tokens
        let (world, commands) = tick(&world, &model_responded(0, usage, StopReason::EndTurn, 1));
        assert!(commands.is_empty());

        let e = world.entities.get(&0).expect("entity");
        assert_eq!(e.budget.spend.used, 100, "spend accumulated");
        assert_eq!(e.budget.context.used, 100, "context refreshed (single-request)");
        assert!(
            e.budget.context.used > e.budget.limits.context_limit,
            "context is over its ceiling"
        );
        assert!(
            e.budget.spend.used < e.budget.limits.spend_limit,
            "spend is under its ceiling"
        );

        // A context overflow is the compaction guard's concern, NOT a spend halt.
        assert_eq!(
            e.gate.halt, None,
            "a context-only overflow does NOT set a spend halt"
        );
        assert!(
            e.gate.is_open(),
            "the entity gate stays open on a context overflow"
        );
    }

    /// A response that keeps spend under the ceiling does not halt; an unbounded
    /// (default `spend_limit == 0`) budget never halts either.
    #[test]
    fn under_limit_and_unbounded_budgets_do_not_halt() {
        // Under an explicit ceiling.
        let budget = Budget {
            limits: Limits {
                spend_limit: 1000,
                context_limit: 0,
                ..Default::default()
            },
            ..Budget::default()
        };
        let world = world_with(Activity::Idle, budget);
        let (world, _) = tick(
            &world,
            &model_responded(
                0,
                Usage {
                    input_tokens: 5,
                    output_tokens: 5,
                },
                StopReason::EndTurn,
                1,
            ),
        );
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(e.gate.halt, None, "under-limit spend does not halt");
        assert!(e.gate.is_open());

        // Unbounded (default limits) never halts even on a large response.
        let world = world_with(Activity::Idle, Budget::default());
        let (world, _) = tick(
            &world,
            &model_responded(
                0,
                Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                },
                StopReason::EndTurn,
                1,
            ),
        );
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(e.gate.halt, None, "an unbounded (limit 0) budget never halts");
        assert!(e.gate.is_open());
    }
}
