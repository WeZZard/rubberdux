//! interaction — correlates `RaiseInteraction` requests with their `InteractionAnswer`.
//! See docs/agent/world/ecs-runtime.md (InteractionSystem; Tick discipline phase 5).
//!
//! InteractionSystem manages the lifecycle of raised interactions:
//!
//! - Pending interactions are recorded in `Resources.raised`, keyed by `request_id`
//!   (populated by whatever System emits the `RaiseInteraction` Command — typically
//!   AutonomySystem). Each entry maps `request_id → (entity, tool_use_id)`, pointing
//!   to the `Local` slot whose `RunTool` is held pending the answer.
//!
//! - On `InteractionAnswer { request_id, answer }`: look up the pending entry by
//!   `request_id`. `Accepted`/`Data` → release the held slot: emit `RunTool` and
//!   record the minted `cmd`. `Rejected` → resolve the slot with `is_error: true`
//!   (B6 — a rejected interaction must not deadlock, Inv 10). In both cases the
//!   entry is removed from `raised` and the all-slots-Done gate is checked via
//!   `tool::advance`.

use super::{tool, Input, System};
use crate::agent::world::autonomy::InteractionResponse;
use crate::agent::world::effects::{Command, CommandKey};
use crate::agent::world::history::{Block, History, Json};
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, ReqId, SlotKind, SlotState, World};

/// InteractionSystem — correlates `RaiseInteraction` ↔ `InteractionAnswer` by
/// `request_id` (phase 5 of the tick).
pub struct InteractionSystem;

impl System for InteractionSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            LogicalInput::InteractionAnswer {
                request_id,
                answer,
            } => apply_answer(world, *request_id, answer),
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// Look up the pending interaction by `request_id` and apply the `answer`.
/// `Accepted`/`Data` → release the held `Local` slot (emit `RunTool`).
/// `Rejected` → resolve the slot with `is_error` (Inv 10, no deadlock).
fn apply_answer(
    world: &World,
    request_id: ReqId,
    answer: &InteractionResponse,
) -> (World, Vec<Command>) {
    // Look up the pending interaction; a missing request_id is a no-op (stale).
    let Some(&(entity_id, ref tool_use_id)) = world.resources.raised.get(&request_id) else {
        return (world.clone(), Vec::new());
    };
    let entity_id = entity_id;
    let tool_use_id = tool_use_id.clone();

    let mut world = world.clone();
    let mut commands: Vec<Command> = Vec::new();

    // Remove the pending entry before acting so a stale re-delivery is a no-op.
    world.resources.raised.remove(&request_id);

    match answer {
        InteractionResponse::Accepted | InteractionResponse::Data(_) => {
            // Approve → release the gated `Local` slot: emit `RunTool` and record cmd.
            let held = world.entities.get(&entity_id).and_then(|e| match &e.activity {
                Activity::ResolvingToolUses { slots } => slots
                    .iter()
                    .find(|s| {
                        s.tool_use_id == tool_use_id
                            && matches!(s.kind, SlotKind::Local)
                            && matches!(s.state, SlotState::Pending { cmd: None })
                    })
                    .and_then(|s| {
                        find_tool_use(&e.history, &s.tool_use_id)
                            .map(|(name, args)| (name.to_string(), args.clone()))
                    }),
                _ => None,
            });

            if let Some((tool, args)) = held {
                let (cmd, ids) = world.resources.ids.mint_cmd();
                world.resources.ids = ids;
                commands.push(Command::RunTool {
                    cmd,
                    entity: entity_id,
                    tool,
                    args,
                    key: CommandKey,
                });
                // Record the minted cmd in the slot so `ToolReturned` can resolve it.
                if let Some(e) = world.entities.get_mut(&entity_id)
                    && let Activity::ResolvingToolUses { slots } = &mut e.activity
                    && let Some(slot) = slots.iter_mut().find(|s| {
                        s.tool_use_id == tool_use_id
                            && matches!(s.kind, SlotKind::Local)
                            && matches!(s.state, SlotState::Pending { cmd: None })
                    })
                {
                    slot.state = SlotState::Pending { cmd: Some(cmd) };
                }
            }
        }
        InteractionResponse::Rejected => {
            // Reject → resolve the slot with is_error (Inv 10, no deadlock).
            if let Some(e) = world.entities.get_mut(&entity_id)
                && let Activity::ResolvingToolUses { slots } = &mut e.activity
                && let Some(slot) = slots.iter_mut().find(|s| {
                    s.tool_use_id == tool_use_id
                        && matches!(s.kind, SlotKind::Local)
                        && matches!(s.state, SlotState::Pending { .. })
                })
            {
                let tuid = slot.tool_use_id.clone();
                slot.result = Some(Block::ToolResult {
                    tool_use_id: tuid,
                    content: vec![Block::Text {
                        text: "action denied by user".into(),
                    }],
                    is_error: true,
                });
                slot.state = SlotState::Done;
            }
        }
    }

    // Check all-slots-Done continuation (shared gate via `tool::advance`).
    let (world, mut advance_cmds) = tool::advance(&world, &entity_id);
    commands.append(&mut advance_cmds);
    (world, commands)
}

/// Look up the `(name, input)` of a `ToolUse` block by `tool_use_id`, used to
/// reconstruct the `RunTool` payload when releasing a held slot.
fn find_tool_use<'a>(history: &'a History, tool_use_id: &str) -> Option<(&'a str, &'a Json)> {
    history
        .0
        .iter()
        .flat_map(|msg| msg.content.iter())
        .find_map(|block| match block {
            Block::ToolUse { id, name, input } if id == tool_use_id => {
                Some((name.as_str(), input))
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use crate::agent::world::autonomy::InteractionResponse;
    use crate::agent::world::effects::Command;
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{Event, LogicalInput, Origin};
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, Components, Effort, Identity, Lineage, ModelConfig, ReqId, Resources, SlotKind,
        SlotState, ToolSlot, Tick, World,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// A World with entity 0 in `ResolvingToolUses` with one `Local` slot in
    /// `Pending{None}` (held for interaction), backed by a `ToolUse` block in
    /// History, and with `resources.raised[request_id] = (0, tool_use_id)`.
    fn held_world(tool_use_id: &str, request_id: ReqId) -> World {
        let mut world = World::new(0, Resources::new(42, model()));
        // Pre-populate history so `find_tool_use` can reconstruct the RunTool.
        let history = History(vec![
            Msg {
                role: Role::User,
                content: vec![Block::Text { text: "go".into() }],
            },
            Msg {
                role: Role::Assistant,
                content: vec![Block::ToolUse {
                    id: tool_use_id.into(),
                    name: "delete_file".into(),
                    input: serde_json::json!({ "path": "/tmp/x" }),
                }],
            },
        ]);
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history,
                activity: Activity::ResolvingToolUses {
                    slots: vec![ToolSlot {
                        tool_use_id: tool_use_id.into(),
                        ordinal: 0,
                        kind: SlotKind::Local,
                        state: SlotState::Pending { cmd: None },
                        result: None,
                    }],
                },
                gate: EntityGate::default(),
                budget: crate::agent::world::budget::Budget::default(),
                inbox: crate::agent::world::world::Inbox::default(),
                turns: 0,
                model: None,
            },
        );
        // Record the pending interaction so InteractionSystem can look it up.
        world
            .resources
            .raised
            .insert(request_id, (0, tool_use_id.into()));
        world
    }

    fn interaction_answer(request_id: ReqId, answer: InteractionResponse, at: Tick) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::InteractionAnswer { request_id, answer },
        }
    }

    /// VC-1.6: `RaiseInteraction` + `InteractionAnswer::Accepted` correlate by
    /// `request_id` and release the held slot (emit `RunTool`).
    #[test]
    fn interaction_answer_accepted_releases_held_slot() {
        let world = held_world("tu_del", 7);

        let (world, cmds) =
            tick(&world, &interaction_answer(7, InteractionResponse::Accepted, 1));

        // The held Local slot must now have a minted cmd (RunTool was emitted).
        let e = world.entities.get(&0).expect("entity");
        let slots = match &e.activity {
            Activity::ResolvingToolUses { slots } => slots,
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        assert!(
            matches!(slots[0].state, SlotState::Pending { cmd: Some(_) }),
            "Accepted → RunTool emitted, slot cmd recorded (Inv 16)"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::RunTool { .. })),
            "exactly one RunTool must be emitted on Accepted"
        );
        // The raised entry is consumed.
        assert!(
            !world.resources.raised.contains_key(&7),
            "raised entry removed after answer"
        );
    }

    /// VC-1.6: `InteractionAnswer::Data` also releases the held slot (treat as approve).
    #[test]
    fn interaction_answer_data_releases_held_slot() {
        let world = held_world("tu_del", 8);

        let (world, cmds) = tick(
            &world,
            &interaction_answer(8, InteractionResponse::Data(serde_json::json!("ok")), 1),
        );

        assert!(
            cmds.iter().any(|c| matches!(c, Command::RunTool { .. })),
            "Data (approve variant) must release the held slot"
        );
        assert!(!world.resources.raised.contains_key(&8));
    }

    /// VC-1.6: `InteractionAnswer::Rejected` resolves the slot with `is_error` — no
    /// deadlock (Inv 10, B6).
    #[test]
    fn interaction_answer_rejected_resolves_as_is_error_no_deadlock() {
        let world = held_world("tu_del", 9);

        let (world, cmds) =
            tick(&world, &interaction_answer(9, InteractionResponse::Rejected, 1));

        // The slot must be Done with is_error; the turn must continue (all-Done gate).
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "Rejected → is_error → turn continues, no deadlock (Inv 10)"
        );
        let last = e.history.0.last().expect("last msg");
        assert!(
            matches!(
                last.content.as_slice(),
                [Block::ToolResult { is_error: true, .. }]
            ),
            "Rejected rides as is_error (Inv 10)"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::CallModel { .. })),
            "continuation CallModel emitted after rejection"
        );
        assert!(!world.resources.raised.contains_key(&9));
    }

    /// A stale `InteractionAnswer` for an unknown `request_id` is a no-op (idempotent).
    #[test]
    fn stale_interaction_answer_is_no_op() {
        let world = held_world("tu_del", 10);

        // Send an answer for a DIFFERENT request_id.
        let (world2, cmds) =
            tick(&world, &interaction_answer(999, InteractionResponse::Accepted, 1));

        // Entities and resources must be unchanged; only `clock` advances (tick discipline).
        assert_eq!(world.entities, world2.entities, "stale answer must leave entities unchanged");
        assert_eq!(
            world.resources, world2.resources,
            "stale answer must leave resources unchanged"
        );
        assert!(cmds.is_empty(), "stale answer emits no commands");
    }
}
