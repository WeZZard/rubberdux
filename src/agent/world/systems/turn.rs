//! turn — the TurnSystem (phase 3 settle). See docs/agent/world/ecs-runtime.md.
//!
//! TurnSystem settles a `Thinking` turn's terminal model result. On `ModelFailed`
//! it returns the entity to `Idle` so `Thinking` is never a sink (Inv 10). On
//! `ModelResponded` it branches on `meta.stop_reason`:
//! - `EndTurn` (the text path) appends the assistant blocks and goes `Idle`.
//! - `ToolUse` branches the ONE assistant turn into `ResolvingToolUses { slots }`,
//!   one `ToolSlot` per tool_use block (this is P1a-tool); ToolSystem then drives
//!   the slots (emit `RunTool`, assemble the continuation).
//!
//! The remaining `MaxTokens`/`Refusal`/`PauseTurn` branches — continuation and
//! retry classification — still settle to `Idle` here until their own tasks land.

use super::{Input, System};
use crate::agent::world::effects::Command;
use crate::agent::world::history::{Block, Msg, Role};
use crate::agent::world::inputs::{LogicalInput, StopReason};
use crate::agent::world::world::{
    Activity, CmdId, EntityId, IdAlloc, SlotKind, SlotState, ToolSlot, World,
};

/// TurnSystem — settles a `Thinking` turn's terminal result (phase 3 of the tick).
pub struct TurnSystem;

impl System for TurnSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            LogicalInput::ModelResponded {
                cmd,
                entity,
                blocks,
                meta,
                ..
            } => {
                if !awaiting(world, entity, *cmd) {
                    return (world.clone(), Vec::new());
                }
                match meta.stop_reason {
                    // A tool-use turn branches into `ResolvingToolUses`: one
                    // `ToolSlot` per tool_use block. ToolSystem (phase 5) then emits
                    // the `RunTool`s and assembles the continuation once every slot
                    // is `Done`. See docs/agent/world/ecs-runtime.md (TurnSystem
                    // `ModelResponded·ToolUse` branch).
                    StopReason::ToolUse => {
                        (begin_resolving_tool_uses(world, entity, blocks), Vec::new())
                    }
                    // `EndTurn` is the designed text path (Thinking→Idle). The
                    // remaining stop reasons (MaxTokens/Refusal/PauseTurn) still
                    // settle to Idle here until their dedicated branches land, so
                    // `Thinking` never becomes a sink (Inv 10); see the design doc.
                    _ => (close_turn(world, entity, Some(blocks.clone())), Vec::new()),
                }
            }
            LogicalInput::ModelFailed { cmd, entity, .. } => {
                if !awaiting(world, entity, *cmd) {
                    return (world.clone(), Vec::new());
                }
                // The failure is already recorded in the event log (log-before-apply,
                // effects.rs); P0 closes the totality gap by returning Thinking→Idle so
                // the entity is not a sink (Inv 10). Human-edge surfacing is workstream
                // U; transient/terminal auto-retry classification is P1a.
                (close_turn(world, entity, None), Vec::new())
            }
            LogicalInput::InferenceCancelled { cmd, entity, .. } => {
                if !awaiting(world, entity, *cmd) {
                    return (world.clone(), Vec::new());
                }
                // A cancelled inference settles a `Thinking` turn → Idle — the
                // totality edge that keeps `Thinking` from becoming a sink (Inv 10)
                // when an in-flight `CallModel` ends without a model result. The
                // `Crash` reason is synthesised by the resume path (effects.rs) after a
                // crash; the entity is never stuck. Autonomy/budget decides any FRESH,
                // accounted retry (new cmd/key) — not here. See
                // docs/agent/world/ecs-runtime.md (Reconciliation; InferenceCancelled).
                (close_turn(world, entity, None), Vec::new())
            }
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// Whether `entity` is currently `Thinking` on exactly `cmd`. The correlation
/// guard that makes settling idempotent: a stray or late result for an
/// already-finalized cmd is dropped, never misrouted (Inv 7).
fn awaiting(world: &World, entity: &EntityId, cmd: CmdId) -> bool {
    world
        .entities
        .get(entity)
        .map(|e| matches!(e.activity, Activity::Thinking { cmd: c } if c == cmd))
        .unwrap_or(false)
}

/// Close the entity's turn: optionally append an assistant `Msg` of `blocks`, then
/// return it to `Idle`. The shared tail of every P0 terminal transition.
fn close_turn(world: &World, entity: &EntityId, blocks: Option<Vec<Block>>) -> World {
    let mut world = world.clone();
    if let Some(e) = world.entities.get_mut(entity) {
        if let Some(blocks) = blocks {
            e.history.0.push(Msg {
                role: Role::Assistant,
                content: blocks,
            });
        }
        e.activity = Activity::Idle;
    }
    world
}

/// Branch a `ToolUse` turn into `ResolvingToolUses`. The assistant turn (with its
/// tool_use blocks) is RECORDED in `History` first — the following `tool_result`
/// user Msg must reference blocks the model already sees (the Anthropic contract).
/// Then one `ToolSlot` is derived per tool_use block, tagged with its `ordinal`
/// (the block's position in the assistant message — the `tool_result` ordering
/// key) and a `SlotKind`. Each slot starts `Pending { cmd: None }`; ToolSystem
/// mints the `cmd` when it emits that slot's `RunTool` (Inv 16).
fn begin_resolving_tool_uses(world: &World, entity: &EntityId, blocks: &[Block]) -> World {
    let mut world = world.clone();
    if !world.entities.contains_key(entity) {
        return world;
    }
    // A sub-agent-spawn block mints a fresh child `EntityId` from the SOLE id minter
    // (`classify_slot_kind` via `&mut ids`). Mint into a LOCAL copy first, written
    // back once, so the borrow of `resources` and the mutable borrow of `entities`
    // below never overlap. `IdAlloc` is `Copy`, so this copy is cheap and pure.
    let mut ids = world.resources.ids;
    let slots: Vec<ToolSlot> = blocks
        .iter()
        .enumerate()
        .filter_map(|(ordinal, block)| match block {
            Block::ToolUse { id, .. } => Some(ToolSlot {
                tool_use_id: id.clone(),
                ordinal: ordinal as u16,
                kind: classify_slot_kind(block, &mut ids),
                state: SlotState::Pending { cmd: None },
                result: None,
            }),
            _ => None,
        })
        .collect();
    world.resources.ids = ids;
    if let Some(e) = world.entities.get_mut(entity) {
        e.history.0.push(Msg {
            role: Role::Assistant,
            content: blocks.to_vec(),
        });
        e.activity = Activity::ResolvingToolUses { slots };
    }
    world
}

/// Classify a tool_use block into the `SlotKind` that resolves it.
///
/// Convention:
/// - `"spawn_subagent"` / `"task"` → `Child(child)` carrying a freshly minted child
///   `EntityId` (SubagentSystem spawns/denies the child Entity under it). The id is
///   minted HERE so the slot's identity is fixed before the child is spawned (Inv 16);
///   ONLY a spawn block draws an entity id — every other kind leaves `ids` untouched.
/// - `"ask_human"` → `Human` (HumanActionSystem emits `RequestHumanAction`).
/// - Everything else → `Local` (ToolSystem emits `RunTool`).
///
/// EXTENSION POINT: when PeerDriveSystem lands, route a peer-drive block → `Peer`
/// here. See docs/agent/world/ecs-runtime.md (TurnSystem `ModelResponded·ToolUse`).
fn classify_slot_kind(block: &Block, ids: &mut IdAlloc) -> SlotKind {
    match block {
        Block::ToolUse { name, .. } if is_subagent_spawn(name) => {
            let (child, next) = ids.mint_entity();
            *ids = next;
            SlotKind::Child(child)
        }
        Block::ToolUse { name, .. } if name == "ask_human" => SlotKind::Human,
        _ => SlotKind::Local,
    }
}

/// Whether a tool name is the sub-agent-spawn tool. Both the canonical
/// `"spawn_subagent"` and the Claude-style `"task"` alias map to a `Child` slot.
fn is_subagent_spawn(name: &str) -> bool {
    name == "spawn_subagent" || name == "task"
}
