//! supervision — the SupervisionSystem: makes a dead in-process child non-blocking
//! (phase 5 TurnAdvance). See docs/agent/world/ecs-runtime.md (SupervisionSystem;
//! child terminal-failure → parent `is_error`).
//!
//! A `Child` slot waits on a child Entity that runs its own turn loop. If that child
//! reaches a TERMINAL FAILURE — it errors out without producing a result — the
//! parent's `Child` slot would wait forever. SupervisionSystem closes that gap: on a
//! child's terminal `ModelFailed` it resolves the PARENT's owning `Child` slot with an
//! `is_error` `ToolResult` and shares the all-slots-`Done` continuation via
//! `tool::advance`, so a dead child can never leave the parent stuck (Inv 10).
//!
//! The child's OWN settling (`Thinking → Idle`) is TurnSystem's job (phase 3); this
//! System only touches the parent's slot, so the two are disjoint. The success path
//! (`ChildReturned`) and the spawn/depth-cap-denial path belong to SubagentSystem.

use super::{Input, System};
use crate::agent::world::effects::Command;
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, SlotKind, SlotState, World};

/// SupervisionSystem — resolves a parent's `Child` slot `is_error` when its child
/// Entity fails terminally (phase 5 of the tick), so the parent is never stuck (Inv 10).
pub struct SupervisionSystem;

impl System for SupervisionSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        // A child's terminal `ModelFailed` is the dead-child signal: TurnSystem (phase
        // 3) settles the CHILD `Thinking → Idle`; here (phase 5) we resolve the
        // PARENT's owning `Child` slot `is_error` so the parent is never stuck waiting
        // on a child that produced no result.
        let LogicalInput::ModelFailed { entity: child, .. } = input else {
            return (world.clone(), Vec::new());
        };
        // The failed entity's parent (if it is an in-process child); a primary entity
        // has no parent, so its own failure is settled by TurnSystem alone, not here.
        let Some(parent) = world.entities.get(child).and_then(|e| e.lineage.parent) else {
            return (world.clone(), Vec::new());
        };

        // Settling is gate-proof (Inv 13): resolve the matching, still-`Pending` `Child`
        // slot `is_error` regardless of any gate.
        let mut world = world.clone();
        let mut resolved = false;
        if let Some(p) = world.entities.get_mut(&parent)
            && let Activity::ResolvingToolUses { slots } = &mut p.activity
            && let Some(slot) = slots.iter_mut().find(|s| {
                matches!(s.kind, SlotKind::Child(c) if c == *child)
                    && matches!(s.state, SlotState::Pending { .. })
            })
        {
            let tuid = slot.tool_use_id.clone();
            slot.result = Some(super::subagent::child_error_result(&tuid, "sub-agent failed"));
            slot.state = SlotState::Done;
            resolved = true;
        }

        // Only advance when a slot actually changed: the shared all-slots-`Done`
        // continuation (gate-guarded inside `advance`) fires once the parent's last
        // slot is resolved.
        if resolved {
            super::tool::advance(&world, &parent)
        } else {
            (world, Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::world::effects::Command;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::Block;
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelError, ModelMeta, Origin,
        ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, EntityId, Identity, Lineage, ModelConfig, Resources,
        SlotKind, SlotState, Tick, World,
    };

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
                history: Default::default(),
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

    fn spawn_subagent_response(cmd: CmdId, tool_use_id: &str, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::ToolUse {
                    id: tool_use_id.into(),
                    name: "spawn_subagent".into(),
                    input: serde_json::json!({ "prompt": "do x" }),
                }],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: "claude-x".into(),
                    stop_reason: StopReason::ToolUse,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    fn model_failed(cmd: CmdId, entity: EntityId, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelFailed {
                cmd,
                entity,
                fingerprint: Fingerprint("fp".into()),
                error: ModelError::Http(503),
            },
        }
    }

    fn turn_cmd_of(commands: &[Command]) -> CmdId {
        match commands.first() {
            Some(Command::CallModel { cmd, .. }) => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    /// [VC-1.4, Inv 10] A child terminal failure (`ModelFailed` for the child Entity)
    /// resolves the parent's owning `Child` slot `is_error` via SupervisionSystem —
    /// the parent is never left waiting on a dead child — and the continuation fires.
    #[test]
    fn child_terminal_failure_resolves_parent_slot_is_error() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, _commands) = tick(&world, &spawn_subagent_response(turn_cmd, "tu_child", 2));

        // Identify the spawned child and the cmd of its in-flight turn.
        let (child, child_cmd) = {
            let slots = match &world.entities.get(&0).expect("parent").activity {
                Activity::ResolvingToolUses { slots } => slots.clone(),
                other => panic!("expected ResolvingToolUses, got {other:?}"),
            };
            let child = match slots[0].kind {
                SlotKind::Child(c) => c,
                other => panic!("expected Child slot, got {other:?}"),
            };
            let cmd = match world.entities.get(&child).expect("child").activity {
                Activity::Thinking { cmd } => cmd,
                ref other => panic!("child Thinking, got {other:?}"),
            };
            (child, cmd)
        };

        // The child errors out terminally.
        let (world, commands) = tick(&world, &model_failed(child_cmd, child, 3));

        // The child itself settled Thinking → Idle (TurnSystem), and the PARENT's Child
        // slot resolved is_error (SupervisionSystem) — the parent advanced, not stuck.
        assert!(matches!(
            world.entities.get(&child).expect("child").activity,
            Activity::Idle
        ));
        let e = world.entities.get(&0).expect("parent");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "the parent advances past the dead child (Inv 10), not stuck"
        );
        assert!(
            commands.iter().any(|c| matches!(c, Command::CallModel { entity: 0, .. })),
            "the all-slots-Done continuation fired for the parent"
        );
        match &e.history.0[2].content[0] {
            Block::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, "tu_child");
                assert!(*is_error, "a dead child resolves its slot is_error (Inv 10)");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// A primary entity's own `ModelFailed` is NOT a child failure: SupervisionSystem
    /// no-ops (TurnSystem settles it), so it never misfires on the root.
    #[test]
    fn primary_model_failed_is_not_treated_as_a_child_failure() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("hi", 1));
        let turn_cmd = turn_cmd_of(&commands);

        let (world, commands) = tick(&world, &model_failed(turn_cmd, 0, 2));

        // The primary settles Thinking → Idle (TurnSystem) with no continuation, and
        // SupervisionSystem adds nothing (no parent slot to resolve).
        assert!(commands.is_empty(), "a primary failure emits no continuation");
        assert!(matches!(
            world.entities.get(&0).expect("primary").activity,
            Activity::Idle
        ));
        // Sanity: the slot-state path is unreachable for the root.
        let _ = SlotState::Done;
    }
}
