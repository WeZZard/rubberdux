//! human_action — owns the `Human` slot kind of a `ResolvingToolUses` turn.
//! See docs/agent/world/ecs-runtime.md (HumanActionSystem; Tick discipline phase 5).
//!
//! HumanActionSystem has two roles over the human-action resolution loop, both
//! driven purely from `(World, Input)`:
//!
//! - **Emit** — for each `Human` `ToolSlot` still `Pending { cmd: None }` it mints
//!   a `cmd` and emits one `RequestHumanAction`, recording that `cmd` in the slot's
//!   `Pending { cmd: Some(cmd) }`. This fires the tick a `ToolUse` turn enters
//!   `ResolvingToolUses` (TurnSystem ran in phase 3 the same tick).
//!
//! - **Settle + advance** — on `HumanActionDone` it records the human result into
//!   the `Human` slot whose `Pending` `cmd` matches and sets it `Done`
//!   (`HumanResult::Provided` → slot carries the JSON answer; `Declined`/`Timeout`
//!   → `is_error: true`, Inv 10 — no deadlock). On `HumanActionAborted` it settles
//!   the matching slot with `is_error` (cancel ack, Inv 10). Once EVERY slot of the
//!   turn is `Done` — across ALL kinds — it calls `tool::advance` to assemble the
//!   shared continuation (`→ Thinking`, emit `CallModel`).

use super::{tool, Input, System};
use crate::agent::world::autonomy::{HumanAction, Notify};
use crate::agent::world::effects::{Command, CommandKey};
use crate::agent::world::history::{Block, History, ToolResult};
use crate::agent::world::inputs::{HumanResult, LogicalInput};
use crate::agent::world::world::{Activity, CmdId, EntityId, SlotKind, SlotState, ToolUseId, World};

/// HumanActionSystem — drives a `ResolvingToolUses` turn's `Human` slots (phase 5).
pub struct HumanActionSystem;

impl System for HumanActionSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // A `ToolUse` turn just entered `ResolvingToolUses` this same tick
            // (TurnSystem, phase 3): emit `RequestHumanAction` for each fresh `Human` slot.
            LogicalInput::ModelResponded { entity, .. } => {
                emit_and_advance(world, entity)
            }
            // Settle the human-action result into the matching `Human` slot.
            LogicalInput::HumanActionDone {
                cmd,
                entity,
                result,
                ..
            } => {
                let world = settle_done(world, entity, *cmd, result);
                tool::advance(&world, entity)
            }
            // Abort ack: settle the matching `Human` slot as is_error (Inv 10).
            LogicalInput::HumanActionAborted { cmd, entity } => {
                let world = settle_is_error(world, entity, *cmd, "human action aborted");
                tool::advance(&world, entity)
            }
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// Emit `RequestHumanAction` for every `Human` slot still `Pending { cmd: None }`,
/// record the minted `cmd` in each slot, then call `tool::advance` to check the
/// all-slots-`Done` gate. Gate-proof for both WorldGate and EntityGate.
fn emit_and_advance(world: &World, entity: &EntityId) -> (World, Vec<Command>) {
    // Gate guard: emitting a new action is NEW WORK — suppress when Closed (Inv 13).
    let entity_gate_open = world
        .entities
        .get(entity)
        .map(|e| e.gate.is_open())
        .unwrap_or(true);
    if !world.resources.gate.is_open() || !entity_gate_open {
        return (world.clone(), Vec::new());
    }

    let mut world = world.clone();
    let mut commands: Vec<Command> = Vec::new();

    // Snapshot pending Human slots (tool_use_id → HumanAction) before mutating.
    let pending: Vec<(ToolUseId, HumanAction)> = match world.entities.get(entity) {
        Some(e) => match &e.activity {
            Activity::ResolvingToolUses { slots } => slots
                .iter()
                .filter(|s| matches!(s.kind, SlotKind::Human))
                .filter(|s| matches!(s.state, SlotState::Pending { cmd: None }))
                .filter_map(|s| {
                    human_action_for(&e.history, &s.tool_use_id)
                        .map(|ask| (s.tool_use_id.clone(), ask))
                })
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };

    // Mint a `cmd` per pending slot, emit its `RequestHumanAction`, and record
    // the `cmd` so the matching `HumanActionDone` resolves it (Inv 16).
    for (tool_use_id, ask) in pending {
        let (cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        commands.push(Command::RequestHumanAction {
            cmd,
            entity: *entity,
            ask,
            notify: Notify::Push,
            key: CommandKey,
        });
        if let Some(e) = world.entities.get_mut(entity)
            && let Activity::ResolvingToolUses { slots } = &mut e.activity
            && let Some(slot) = slots.iter_mut().find(|s| {
                s.tool_use_id == tool_use_id && matches!(s.kind, SlotKind::Human)
            })
        {
            slot.state = SlotState::Pending { cmd: Some(cmd) };
        }
    }

    // Check all-slots-Done continuation (shared with ToolSystem via `tool::advance`).
    let (world, mut advance_cmds) = tool::advance(&world, entity);
    commands.append(&mut advance_cmds);
    (world, commands)
}

/// Settle a `HumanActionDone` into the `Human` slot whose `Pending { cmd }` matches.
/// `Provided` → the JSON answer becomes the slot's `ToolResult`; `Declined`/`Timeout`
/// → `is_error: true` (Inv 10, no deadlock).
fn settle_done(world: &World, entity: &EntityId, cmd: CmdId, result: &HumanResult) -> World {
    let mut world = world.clone();
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| {
            matches!(s.kind, SlotKind::Human)
                && matches!(s.state, SlotState::Pending { cmd: Some(c) } if c == cmd)
        })
    {
        let tuid = slot.tool_use_id.clone();
        slot.result = Some(match result {
            HumanResult::Provided(value) => Block::ToolResult {
                tool_use_id: tuid,
                content: vec![Block::Text {
                    text: value.to_string(),
                }],
                is_error: false,
            },
            HumanResult::Declined => error_result(tuid, "human action declined"),
            HumanResult::Timeout => error_result(tuid, "human action timed out"),
        });
        slot.state = SlotState::Done;
    }
    world
}

/// Settle a `HumanActionAborted` (cancel ack) into the matching `Human` slot as
/// `is_error` so the turn can proceed without deadlocking (Inv 10).
fn settle_is_error(world: &World, entity: &EntityId, cmd: CmdId, reason: &str) -> World {
    let mut world = world.clone();
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| {
            matches!(s.kind, SlotKind::Human)
                && matches!(s.state, SlotState::Pending { cmd: Some(c) } if c == cmd)
        })
    {
        let tuid = slot.tool_use_id.clone();
        slot.result = Some(error_result(tuid, reason));
        slot.state = SlotState::Done;
    }
    world
}

/// Build an `is_error` `ToolResult` block for denial / abort / timeout paths (Inv 10).
fn error_result(tool_use_id: ToolUseId, text: &str) -> ToolResult {
    Block::ToolResult {
        tool_use_id,
        content: vec![Block::Text { text: text.into() }],
        is_error: true,
    }
}

/// Extract the `HumanAction` a slot backs from the recorded `History`. The slot
/// carries only the `tool_use_id`; the dispatch payload lives on the assistant
/// turn's `ToolUse` block. Derives `HumanAction::Prompt { text }` from the
/// block's `input.text` field, falling back to an empty string.
fn human_action_for(history: &History, tool_use_id: &str) -> Option<HumanAction> {
    history
        .0
        .iter()
        .flat_map(|msg| msg.content.iter())
        .find_map(|block| match block {
            Block::ToolUse { id, input, .. } if id == tool_use_id => {
                let text = input
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                Some(HumanAction::Prompt { text })
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use crate::agent::world::effects::Command;
    use crate::agent::world::history::{Block, History, Json, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, HumanResult, LogicalInput, ModelMeta, Origin,
        ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, SlotKind,
        SlotState, ToolSlot, Tick, World,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    fn idle_world() -> World {
        let mut world = World::new(0, Resources::new(42, model()));
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
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );
        world
    }

    /// A World already in `ResolvingToolUses` with ONE `Human` slot in `Pending{None}`,
    /// backed by a `ToolUse` block in History. Used for settle-only tests that skip
    /// the ModelResponded/emit path.
    fn resolving_human(tool_use_id: &str, ordinal: u16) -> World {
        let mut world = idle_world();
        let e = world.entities.get_mut(&0).expect("entity");
        e.history.0.push(Msg {
            role: Role::User,
            content: vec![Block::Text { text: "go".into() }],
        });
        e.history.0.push(Msg {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: tool_use_id.into(),
                name: "ask_human".into(),
                input: serde_json::json!({ "text": "Please confirm." }),
            }],
        });
        e.activity = Activity::ResolvingToolUses {
            slots: vec![ToolSlot {
                tool_use_id: tool_use_id.into(),
                ordinal,
                kind: SlotKind::Human,
                state: SlotState::Pending { cmd: None },
                result: None,
            }],
        };
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

    fn model_responded_human(call_cmd: CmdId, tool_use_id: &str, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd: call_cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::ToolUse {
                    id: tool_use_id.into(),
                    name: "ask_human".into(),
                    input: serde_json::json!({ "text": "Please confirm." }),
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

    fn human_action_done(cmd: CmdId, result: HumanResult, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::HumanActionDone {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp-done".into()),
                result,
            },
        }
    }

    fn human_action_aborted(cmd: CmdId, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::HumanActionAborted { cmd, entity: 0 },
        }
    }

    fn call_model_cmd(commands: &[Command]) -> CmdId {
        match commands.first() {
            Some(Command::CallModel { cmd, .. }) => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    fn request_human_action_cmd(commands: &[Command]) -> CmdId {
        commands
            .iter()
            .find_map(|c| match c {
                Command::RequestHumanAction { cmd, .. } => Some(*cmd),
                _ => None,
            })
            .expect("expected RequestHumanAction")
    }

    /// VC-1.6: a `Human` slot in `Pending{None}` emits one `RequestHumanAction`
    /// and records the minted `cmd` (slot identity is the `cmd`, Inv 16).
    #[test]
    fn human_slot_emits_request_human_action_and_records_cmd() {
        let world = idle_world();
        let (world, cmds) = tick(&world, &user_message("hi", 1));
        let turn_cmd = call_model_cmd(&cmds);
        let (world, cmds) = tick(&world, &model_responded_human(turn_cmd, "tu_h", 2));

        // HumanActionSystem must emit exactly one RequestHumanAction.
        let req_cmds: Vec<&Command> = cmds
            .iter()
            .filter(|c| matches!(c, Command::RequestHumanAction { .. }))
            .collect();
        assert_eq!(req_cmds.len(), 1, "one RequestHumanAction per Human slot");

        // The slot's cmd must be recorded.
        let e = world.entities.get(&0).expect("entity");
        let slots = match &e.activity {
            Activity::ResolvingToolUses { slots } => slots,
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        assert_eq!(slots.len(), 1);
        assert!(matches!(slots[0].kind, SlotKind::Human));
        assert!(
            matches!(slots[0].state, SlotState::Pending { cmd: Some(_) }),
            "cmd must be recorded in the slot (Inv 16)"
        );
    }

    /// VC-1.6: `HumanActionDone::Provided` resolves the slot; the turn continues.
    #[test]
    fn human_action_done_provided_resolves_slot_and_continues_turn() {
        let world = idle_world();
        let (world, cmds) = tick(&world, &user_message("hi", 1));
        let turn_cmd = call_model_cmd(&cmds);
        let (world, cmds) = tick(&world, &model_responded_human(turn_cmd, "tu_h", 2));
        let ha_cmd = request_human_action_cmd(&cmds);

        let (world, cmds) = tick(
            &world,
            &human_action_done(ha_cmd, HumanResult::Provided(serde_json::json!("yes")), 3),
        );

        // The slot is Done → one user Msg appended, entity → Thinking, one CallModel.
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "Provided → slot Done → turn continues (Inv 10)"
        );
        assert_eq!(e.history.0.len(), 3, "user + assistant(tool_use) + user(tool_result)");
        let last = &e.history.0[2];
        assert!(
            matches!(last.content.as_slice(), [Block::ToolResult { is_error: false, .. }]),
            "Provided result is NOT is_error"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::CallModel { .. })),
            "continuation CallModel emitted"
        );
    }

    /// VC-1.6: `HumanActionDone::Declined` resolves the slot with `is_error`; no deadlock.
    #[test]
    fn human_action_done_declined_resolves_as_is_error_no_deadlock() {
        let world = idle_world();
        let (world, cmds) = tick(&world, &user_message("hi", 1));
        let turn_cmd = call_model_cmd(&cmds);
        let (world, cmds) = tick(&world, &model_responded_human(turn_cmd, "tu_h", 2));
        let ha_cmd = request_human_action_cmd(&cmds);

        let (world, cmds) = tick(&world, &human_action_done(ha_cmd, HumanResult::Declined, 3));

        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "Declined → is_error → turn continues, no deadlock (Inv 10)"
        );
        let last = &e.history.0[2];
        assert!(
            matches!(last.content.as_slice(), [Block::ToolResult { is_error: true, .. }]),
            "Declined rides as is_error (Inv 10)"
        );
        assert!(cmds.iter().any(|c| matches!(c, Command::CallModel { .. })));
    }

    /// VC-1.6: `HumanActionDone::Timeout` resolves the slot with `is_error`; no deadlock.
    #[test]
    fn human_action_done_timeout_resolves_as_is_error_no_deadlock() {
        let world = idle_world();
        let (world, cmds) = tick(&world, &user_message("hi", 1));
        let turn_cmd = call_model_cmd(&cmds);
        let (world, cmds) = tick(&world, &model_responded_human(turn_cmd, "tu_h", 2));
        let ha_cmd = request_human_action_cmd(&cmds);

        let (world, cmds) = tick(&world, &human_action_done(ha_cmd, HumanResult::Timeout, 3));

        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "Timeout → is_error → turn continues, no deadlock (Inv 10)"
        );
        let last = &e.history.0[2];
        assert!(
            matches!(last.content.as_slice(), [Block::ToolResult { is_error: true, .. }]),
            "Timeout rides as is_error (Inv 10)"
        );
        assert!(cmds.iter().any(|c| matches!(c, Command::CallModel { .. })));
    }

    /// VC-1.6: `HumanActionAborted` (cancel ack) resolves the slot with `is_error`
    /// so the turn can settle rather than deadlocking (Inv 10).
    #[test]
    fn human_action_aborted_resolves_as_is_error_no_deadlock() {
        // Use the pre-built resolving world for settle-only tests.
        let mut world = resolving_human("tu_h", 0);
        // Mint the cmd manually (simulating what emit_and_advance would have done).
        let (cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        if let Some(e) = world.entities.get_mut(&0)
            && let Activity::ResolvingToolUses { slots } = &mut e.activity
        {
            slots[0].state = SlotState::Pending { cmd: Some(cmd) };
        }

        let (world, cmds) = tick(&world, &human_action_aborted(cmd, 3));

        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { .. }),
            "HumanActionAborted → is_error → turn continues, no deadlock (Inv 10)"
        );
        let last = e.history.0.last().expect("last msg");
        assert!(
            matches!(last.content.as_slice(), [Block::ToolResult { is_error: true, .. }]),
            "abort ack rides as is_error (Inv 10)"
        );
        assert!(cmds.iter().any(|c| matches!(c, Command::CallModel { .. })));
    }
}
