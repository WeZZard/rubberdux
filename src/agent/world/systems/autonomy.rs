//! autonomy — AutonomySystem: tier-gated tool approval before dispatch.
//! See docs/agent/world/ecs-runtime.md (AutonomySystem; Autonomy/Tier; Inv 10).
//!
//! AutonomySystem owns the tier-approval decision for `Local` tool slots and
//! coordinates with ToolSystem and InteractionSystem via two pure World mutations:
//!
//! **Phase 5, before ToolSystem — on `ModelResponded`:**
//! TurnSystem (phase 3) just created `ResolvingToolUses` slots in `Pending { cmd: None }`.
//! AutonomySystem inspects each `Local Pending { cmd: None }` slot under the
//! world-default `Autonomy` policy. Slots requiring approval are set to
//! `Pending { cmd: Some(HELD_CMD) }` (preventing ToolSystem dispatch), a
//! `RaiseInteraction` is emitted, and the pending request is recorded in
//! `Resources.raised`. Slots not requiring approval are left untouched for
//! ToolSystem to dispatch normally.
//!
//! **Phase 5, before InteractionSystem — on `InteractionAnswer`:**
//! AutonomySystem finds any slot it gated (`Pending { cmd: Some(HELD_CMD) }`) for the
//! answered `request_id` and resets it to `Pending { cmd: None }`. This restores the
//! slot to the shape InteractionSystem's existing release mechanism expects:
//! `Accepted`/`Data` → InteractionSystem emits `RunTool` and records its minted cmd;
//! `Rejected` → InteractionSystem marks the slot `Done is_error` (Inv 10, no deadlock).
//! AutonomySystem does NOT remove the `raised` entry; InteractionSystem does.
//!
//! **On `SetAutonomy`:** updates `Resources.autonomy` (the world-default policy).
//!
//! P1a simplification for tier gating: `GateTier(FlagReversible | BlockIrreversible)`
//! gates ALL Local slots, because tool reversibility classification is a later milestone
//! and the safe default is to require approval for every tool action that would otherwise
//! run unreviewed. `GateTier(FreeRun)` and `RunFree` never gate. `AskEverything` gates all.

use super::{Input, System};
use crate::agent::world::autonomy::{AgentInteraction, Autonomy, Tier};
use crate::agent::world::effects::Command;
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, EntityId, SlotKind, SlotState, World, HELD_CMD};

/// AutonomySystem — gates `Local` tool slots requiring approval under the entity's
/// `Autonomy` policy (phase 5 of the tick, before ToolSystem and InteractionSystem).
/// See docs/agent/world/ecs-runtime.md (AutonomySystem; Tick discipline phase 5).
pub struct AutonomySystem;

impl System for AutonomySystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // Update the world-default autonomy policy (EXOGENOUS).
            LogicalInput::SetAutonomy { policy } => {
                let mut world = world.clone();
                world.resources.autonomy = policy.clone();
                (world, Vec::new())
            }
            // Gate Local slots BEFORE ToolSystem dispatches them.
            LogicalInput::ModelResponded { entity, .. } => gate_new_slots(world, *entity),
            // Reset HELD_CMD slots BEFORE InteractionSystem releases them.
            LogicalInput::InteractionAnswer { request_id, .. } => {
                restore_held_slot(world, *request_id)
            }
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// Whether the current `Autonomy` policy requires approval before any Local tool action.
/// P1a simplification: `GateTier(FlagReversible | BlockIrreversible)` gates all actions
/// (reversibility classification is a later milestone; gating all is the safe default).
fn requires_approval(policy: &Autonomy) -> bool {
    match policy {
        Autonomy::AskEverything => true,
        Autonomy::GateTier(tier) => !matches!(tier, Tier::FreeRun),
        Autonomy::RunFree => false,
    }
}

/// On `ModelResponded`, gate every `Local Pending { cmd: None }` slot of `entity`
/// under the world-default policy: mark `Pending { cmd: Some(HELD_CMD) }`, emit
/// `RaiseInteraction`, and record the pending request in `Resources.raised`.
fn gate_new_slots(world: &World, entity: EntityId) -> (World, Vec<Command>) {
    if !requires_approval(&world.resources.autonomy) {
        return (world.clone(), Vec::new());
    }

    // Collect tool_use_ids to gate before mutating the world.
    let to_gate: Vec<String> = world
        .entities
        .get(&entity)
        .and_then(|e| match &e.activity {
            Activity::ResolvingToolUses { slots } => Some(
                slots
                    .iter()
                    .filter(|s| {
                        matches!(s.kind, SlotKind::Local)
                            && matches!(s.state, SlotState::Pending { cmd: None })
                    })
                    .map(|s| s.tool_use_id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .unwrap_or_default();

    if to_gate.is_empty() {
        return (world.clone(), Vec::new());
    }

    let mut world = world.clone();
    let mut commands: Vec<Command> = Vec::new();

    for tool_use_id in to_gate {
        // Mark the slot HELD_CMD so ToolSystem's `Pending { cmd: None }` filter skips it.
        if let Some(e) = world.entities.get_mut(&entity)
            && let Activity::ResolvingToolUses { slots } = &mut e.activity
            && let Some(slot) = slots.iter_mut().find(|s| {
                s.tool_use_id == tool_use_id
                    && matches!(s.kind, SlotKind::Local)
                    && matches!(s.state, SlotState::Pending { cmd: None })
            })
        {
            slot.state = SlotState::Pending { cmd: Some(HELD_CMD) };
        }

        // Mint a request_id for this approval and record it so InteractionSystem
        // can correlate the later `InteractionAnswer` back to (entity, tool_use_id).
        let (request_id, ids) = world.resources.ids.mint_request();
        world.resources.ids = ids;
        world
            .resources
            .raised
            .insert(request_id, (entity, tool_use_id.clone()));

        // Emit the approval request. The `kind: approval` payload is the P1a shape;
        // the full structured payload is a later milestone.
        commands.push(Command::RaiseInteraction {
            request_id,
            entity,
            interaction: AgentInteraction {
                payload: serde_json::json!({
                    "kind": "approval",
                    "tool_use_id": tool_use_id
                }),
            },
        });
    }

    (world, commands)
}

/// On `InteractionAnswer`, if the answered request maps to a slot in the HELD_CMD state
/// (`Pending { cmd: Some(HELD_CMD) }`), reset it to `Pending { cmd: None }` so
/// InteractionSystem's existing release mechanism can find and process it. The
/// `Resources.raised` entry is left intact; InteractionSystem removes it on answer.
fn restore_held_slot(world: &World, request_id: u32) -> (World, Vec<Command>) {
    let Some(&(entity_id, ref tool_use_id)) = world.resources.raised.get(&request_id) else {
        return (world.clone(), Vec::new());
    };
    let tool_use_id = tool_use_id.clone();

    // Only reset if the slot was gated by AutonomySystem (carries the HELD_CMD sentinel).
    // A request_id in `raised` that was NOT raised by AutonomySystem (e.g., a future
    // System that also uses `RaiseInteraction`) will have a different slot state and
    // must not be touched here.
    let is_autonomy_gated = world
        .entities
        .get(&entity_id)
        .map_or(false, |e| match &e.activity {
            Activity::ResolvingToolUses { slots } => slots.iter().any(|s| {
                s.tool_use_id == tool_use_id
                    && matches!(s.kind, SlotKind::Local)
                    && matches!(s.state, SlotState::Pending { cmd: Some(HELD_CMD) })
            }),
            _ => false,
        });

    if !is_autonomy_gated {
        return (world.clone(), Vec::new());
    }

    let mut world = world.clone();
    if let Some(e) = world.entities.get_mut(&entity_id)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| {
            s.tool_use_id == tool_use_id
                && matches!(s.kind, SlotKind::Local)
                && matches!(s.state, SlotState::Pending { cmd: Some(HELD_CMD) })
        })
    {
        slot.state = SlotState::Pending { cmd: None };
    }

    (world, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::autonomy::{Autonomy, Tier};
    use crate::agent::world::effects::Command;
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, SlotKind,
        SlotState, Tick, ToolSlot, World, HELD_CMD,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// Build a base world with entity 0 at `Idle`, ready for a turn.
    fn idle_world_with_policy(policy: Autonomy) -> World {
        let mut world = World::new(0, Resources::new(42, model()));
        world.resources.autonomy = policy;
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History::default(),
                activity: Activity::Idle,
                gate: EntityGate::default(),
                budget: crate::agent::world::budget::Budget::default(),
                inbox: crate::agent::world::world::Inbox::default(),
                turns: 0,
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
            input: LogicalInput::UserMessage { to: 0, text: text.into() },
        }
    }

    /// A `ModelResponded` with `stop_reason == ToolUse` and a single `tool_use` block.
    fn model_responded_tool_use(cmd: CmdId, tool_use_id: &str, tool_name: &str, at: Tick) -> Event {
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
                    name: tool_name.into(),
                    input: serde_json::json!({ "arg": 1 }),
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

    fn call_model_cmd(command: &Command) -> CmdId {
        match command {
            Command::CallModel { cmd, .. } => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    fn interaction_answer(
        request_id: u32,
        answer: crate::agent::world::autonomy::InteractionResponse,
        at: Tick,
    ) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::InteractionAnswer { request_id, answer },
        }
    }

    /// VC-1.6: under a gating `Autonomy` policy, a tiered tool action emits
    /// `RaiseInteraction(Approval)` and does NOT dispatch `RunTool`. The slot
    /// is held in `Pending { cmd: Some(HELD_CMD) }` and `Resources.raised` is set.
    #[test]
    fn gating_policy_raises_interaction_and_holds_run_tool() {
        let world = idle_world_with_policy(Autonomy::GateTier(Tier::BlockIrreversible));

        // Drive a user message to start a turn.
        let (world, cmds) = tick(&world, &user_message("go", 1));
        let turn_cmd = call_model_cmd(&cmds[0]);

        // Model responds with a tool_use; AutonomySystem should gate it.
        let (world, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_1", "delete_file", 2));

        // MUST emit RaiseInteraction.
        assert!(
            cmds.iter().any(|c| matches!(c, Command::RaiseInteraction { .. })),
            "gating policy must emit RaiseInteraction"
        );
        // MUST NOT emit RunTool (slot is held).
        assert!(
            !cmds.iter().any(|c| matches!(c, Command::RunTool { .. })),
            "gating policy must NOT dispatch RunTool before approval"
        );
        // Slot must be in the HELD_CMD state.
        let e = world.entities.get(&0).expect("entity");
        let slots = match &e.activity {
            Activity::ResolvingToolUses { slots } => slots,
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        assert!(
            matches!(slots[0].state, SlotState::Pending { cmd: Some(HELD_CMD) }),
            "slot must be in HELD_CMD state after AutonomySystem gates it"
        );
        // Resources.raised must have the pending request.
        assert!(!world.resources.raised.is_empty(), "raised must record the pending approval");
    }

    /// VC-1.6: `AskEverything` also gates every action.
    #[test]
    fn ask_everything_gates_every_action() {
        let world = idle_world_with_policy(Autonomy::AskEverything);
        let (world, cmds) = tick(&world, &user_message("go", 1));
        let turn_cmd = call_model_cmd(&cmds[0]);
        let (world, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_a", "tool_a", 2));

        assert!(cmds.iter().any(|c| matches!(c, Command::RaiseInteraction { .. })));
        assert!(!cmds.iter().any(|c| matches!(c, Command::RunTool { .. })));
        assert!(!world.resources.raised.is_empty());
    }

    /// VC-1.6: an `InteractionAnswer::Accepted` releases the held slot — `RunTool` is
    /// emitted and the slot transitions to `Pending { cmd: Some(real_cmd) }`.
    #[test]
    fn interaction_answer_accepted_releases_held_slot_and_emits_run_tool() {
        let world = idle_world_with_policy(Autonomy::GateTier(Tier::BlockIrreversible));
        let (world, cmds) = tick(&world, &user_message("go", 1));
        let turn_cmd = call_model_cmd(&cmds[0]);
        let (world, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_1", "delete_file", 2));

        // Extract the request_id from the RaiseInteraction command.
        let request_id = cmds
            .iter()
            .find_map(|c| match c {
                Command::RaiseInteraction { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .expect("RaiseInteraction must carry a request_id");

        // Approve the interaction.
        let (world, cmds) = tick(
            &world,
            &interaction_answer(request_id, crate::agent::world::autonomy::InteractionResponse::Accepted, 3),
        );

        // RunTool must be emitted after approval.
        assert!(
            cmds.iter().any(|c| matches!(c, Command::RunTool { .. })),
            "Accepted → RunTool must be emitted"
        );
        // Slot must now have a real (non-HELD_CMD) cmd.
        let e = world.entities.get(&0).expect("entity");
        let slots = match &e.activity {
            Activity::ResolvingToolUses { slots } => slots,
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        assert!(
            matches!(slots[0].state, SlotState::Pending { cmd: Some(c) } if c != HELD_CMD),
            "Accepted → slot must have a real minted cmd, not HELD_CMD"
        );
        // raised entry consumed by InteractionSystem.
        assert!(
            !world.resources.raised.contains_key(&request_id),
            "raised entry must be removed after answer"
        );
    }

    /// VC-1.6: `InteractionAnswer::Rejected` resolves the slot `is_error` (no deadlock, Inv 10).
    #[test]
    fn interaction_answer_rejected_resolves_slot_is_error_no_deadlock() {
        let world = idle_world_with_policy(Autonomy::GateTier(Tier::BlockIrreversible));
        let (world, cmds) = tick(&world, &user_message("go", 1));
        let turn_cmd = call_model_cmd(&cmds[0]);
        let (world, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_1", "delete_file", 2));

        let request_id = cmds
            .iter()
            .find_map(|c| match c {
                Command::RaiseInteraction { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .expect("RaiseInteraction must carry a request_id");

        // Reject the interaction.
        let (world, cmds) = tick(
            &world,
            &interaction_answer(request_id, crate::agent::world::autonomy::InteractionResponse::Rejected, 3),
        );

        // The entity must advance to Thinking (the only slot is Done → continuation fires, Inv 10).
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "Rejected → is_error → slot Done → continuation (no deadlock, Inv 10)"
        );
        // Continuation CallModel must be emitted.
        assert!(
            cmds.iter().any(|c| matches!(c, Command::CallModel { .. })),
            "continuation CallModel emitted after rejection"
        );
        // History tail is an is_error ToolResult.
        let last = e.history.0.last().expect("last msg");
        assert!(
            matches!(
                last.content.as_slice(),
                [Block::ToolResult { is_error: true, .. }]
            ),
            "rejection resolves slot as is_error in History (Inv 10)"
        );
        assert!(
            !world.resources.raised.contains_key(&request_id),
            "raised entry must be removed after answer"
        );
    }

    /// A `RunFree` policy dispatches `RunTool` directly — no `RaiseInteraction`.
    #[test]
    fn run_free_dispatches_run_tool_directly_no_interaction() {
        let world = idle_world_with_policy(Autonomy::RunFree);
        let (world, cmds) = tick(&world, &user_message("go", 1));
        let turn_cmd = call_model_cmd(&cmds[0]);
        let (_, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_1", "do_thing", 2));

        assert!(
            cmds.iter().any(|c| matches!(c, Command::RunTool { .. })),
            "RunFree → RunTool dispatched directly"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Command::RaiseInteraction { .. })),
            "RunFree → no RaiseInteraction"
        );
    }

    /// `GateTier(FreeRun)` also dispatches directly (tier says free run).
    #[test]
    fn gate_tier_free_run_dispatches_directly() {
        let world = idle_world_with_policy(Autonomy::GateTier(Tier::FreeRun));
        let (world, cmds) = tick(&world, &user_message("go", 1));
        let turn_cmd = call_model_cmd(&cmds[0]);
        let (_, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_1", "do_thing", 2));

        assert!(cmds.iter().any(|c| matches!(c, Command::RunTool { .. })));
        assert!(!cmds.iter().any(|c| matches!(c, Command::RaiseInteraction { .. })));
    }

    /// `SetAutonomy` updates the world-default policy for subsequent actions.
    #[test]
    fn set_autonomy_updates_world_default_policy() {
        // Start RunFree, then switch to AskEverything, then verify gating applies.
        let world = idle_world_with_policy(Autonomy::RunFree);

        let set_event = Event {
            origin: Origin::Human,
            edge: 0,
            at: 1,
            wall: None,
            input: LogicalInput::SetAutonomy { policy: Autonomy::AskEverything },
        };
        let (world, _) = tick(&world, &set_event);
        assert_eq!(world.resources.autonomy, Autonomy::AskEverything);

        let (world, cmds) = tick(&world, &user_message("go", 2));
        let turn_cmd = call_model_cmd(&cmds[0]);
        let (_, cmds) = tick(&world, &model_responded_tool_use(turn_cmd, "tu_1", "do_thing", 3));

        assert!(cmds.iter().any(|c| matches!(c, Command::RaiseInteraction { .. })));
        assert!(!cmds.iter().any(|c| matches!(c, Command::RunTool { .. })));
    }
}
