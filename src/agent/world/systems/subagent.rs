//! subagent — the SubagentSystem: owns the `Child` slot kind of a
//! `ResolvingToolUses` turn (phase 5 TurnAdvance). See
//! docs/agent/world/ecs-runtime.md (SubagentSystem; in-process sub-agents).
//!
//! A `Child` slot models an IN-PROCESS sub-agent: a child Entity living in the SAME
//! `World`, running its own turn loop. SubagentSystem has three roles, all pure
//! functions of `(World, Input)`:
//!
//! - **Spawn / deny** — on the `ModelResponded` that branched a `ToolUse` turn into
//!   `ResolvingToolUses` (TurnSystem, phase 3), for each fresh `Child` slot it either
//!   SPAWNS the child Entity (lineage = parent, depth = parent.depth + 1, seeded with
//!   the sub-agent prompt and a `Thinking` turn + its `CallModel`) or, if the child
//!   would exceed `Resources.depth_cap`, DENIES inline by resolving the slot with an
//!   `is_error` `ToolResult` WITHOUT spawning (Inv 10 — a depth-capped spawn never
//!   leaves the parent waiting).
//! - **Settle** — on `ChildReturned { parent, child, tool_use_id, result }` it records
//!   the child's result into the matching `Child` slot and sets it `Done`. The slot is
//!   correlated by IDENTITY `(child, tool_use_id)` — NOT by a `cmd` (a child has no
//!   shell-dispatched command; Inv 16).
//! - **Advance** — after a spawn/deny or a settle it shares the all-slots-`Done`
//!   continuation with ToolSystem via `tool::advance` (one user `Msg` of every slot's
//!   result in ascending `ordinal` order, `→ Thinking`, emit `CallModel`).
//!
//! A child TERMINAL FAILURE (the dead-child case) is owned by SupervisionSystem, not
//! here, so this module stays the success / depth-cap-denial path.

use super::{Input, System};
use crate::agent::world::budget::Budget;
use crate::agent::world::effects::{Command, CommandKey, ToolSet};
use crate::agent::world::gates::EntityGate;
use crate::agent::world::history::{Block, History, Msg, Role, ToolResult};
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{
    Activity, Components, EntityId, Identity, Inbox, Lineage, SlotKind, SlotState, ToolUseId, World,
};

/// SubagentSystem — drives a `ResolvingToolUses` turn's `Child` slots (phase 5 of the
/// tick): spawn (or depth-cap-deny) on the branching `ModelResponded`, settle each
/// `ChildReturned` by `(child, tool_use_id)`, and continue once every slot is `Done`.
pub struct SubagentSystem;

impl System for SubagentSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // The tick a `ToolUse` turn entered `ResolvingToolUses` (TurnSystem, phase
            // 3): spawn or depth-cap-deny each fresh `Child` slot, then share the
            // all-Done continuation (fires now iff every spawn was denied).
            LogicalInput::ModelResponded { entity, .. } => spawn_or_deny(world, entity),
            // A child completed: settle its result into the matching `Child` slot
            // (correlated by `(child, tool_use_id)`, Inv 16), then advance.
            LogicalInput::ChildReturned {
                parent,
                child,
                tool_use_id,
                result,
            } => {
                let (world, resolved) =
                    settle_child_returned(world, parent, *child, tool_use_id, result);
                if resolved {
                    super::tool::advance(&world, parent)
                } else {
                    (world, Vec::new())
                }
            }
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// Spawn (or depth-cap-deny) every fresh `Child` slot of `parent`'s
/// `ResolvingToolUses` turn, then share the all-slots-`Done` continuation.
fn spawn_or_deny(world: &World, parent: &EntityId) -> (World, Vec<Command>) {
    // Gate guard (Inv 13): spawning a child and emitting its `CallModel` is NEW WORK,
    // so it requires BOTH the App-wide `WorldGate` and the parent's `EntityGate` Open.
    // Settling a returned child (`ChildReturned` / SupervisionSystem) is gate-proof.
    if !gates_open(world, parent) {
        return (world.clone(), Vec::new());
    }

    // Snapshot the parent depth and the `Child` slots still needing a spawn decision
    // (Child kind, still `Pending { cmd: None }`, child Entity NOT yet spawned) BEFORE
    // mutating — so the read-borrow is released before the mint/insert below.
    let parent_depth = match world.entities.get(parent) {
        Some(e) => e.lineage.depth,
        None => return (world.clone(), Vec::new()),
    };
    let pending: Vec<(ToolUseId, EntityId)> = match world.entities.get(parent) {
        Some(e) => match &e.activity {
            Activity::ResolvingToolUses { slots } => slots
                .iter()
                .filter_map(|s| match s.kind {
                    SlotKind::Child(child)
                        if matches!(s.state, SlotState::Pending { cmd: None })
                            && !world.entities.contains_key(&child) =>
                    {
                        Some((s.tool_use_id.clone(), child))
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    if pending.is_empty() {
        return (world.clone(), Vec::new());
    }

    let mut world = world.clone();
    let mut commands = Vec::new();
    let default_model = world.resources.model.clone();
    let depth_cap = world.resources.depth_cap;
    // Fan-out cap (Boundedness, Inv 11): `depth_cap` bounds NESTING along a path;
    // `fanout_cap` bounds the BREADTH a single entity spawns — independent brakes.
    // The breadth is tracked by the CUMULATIVE `Components.spawned` counter (bumped
    // on each spawn, NEVER decremented on child completion), so a finished child does
    // NOT refund budget. `0` ⇒ unbounded.
    let fanout_cap = world
        .entities
        .get(parent)
        .map(|e| e.budget.limits.fanout_cap)
        .unwrap_or(0);
    // Fan-out cap (Boundedness, Inv 11): the parent's CUMULATIVE `spawned` counter vs
    // its `fanout_cap`. Unlike a live-count, `spawned` is bumped on each spawn and
    // NEVER decremented when a child completes or is removed — a finished child does NOT
    // refund the budget (design §1274–1278). `0` ⇒ unbounded. See
    // docs/agent/world/ecs-runtime.md (Fan-out budget — bound sub-agent spawning).
    let mut cumulative_spawned = world
        .entities
        .get(parent)
        .map(|e| e.spawned)
        .unwrap_or(0);

    for (tool_use_id, child) in pending {
        let child_depth = parent_depth.saturating_add(1);
        if child_depth > depth_cap {
            // DENY (Inv 10): resolve the slot inline `is_error`, do NOT spawn — a
            // depth-capped spawn must never leave the parent waiting on a child that
            // will never exist.
            resolve_slot(
                &mut world,
                parent,
                &tool_use_id,
                child_error_result(&tool_use_id, "sub-agent depth cap reached"),
            );
            continue;
        }
        // Fan-out DENY (Inv 10/11): a spawn past the cumulative cap resolves the slot
        // `is_error` WITHOUT spawning — MIRRORING the depth-cap deny — so the parent
        // never waits on a child that will never exist (B6 no-deadlock rule).
        if fanout_cap != 0 && cumulative_spawned >= fanout_cap {
            resolve_slot(
                &mut world,
                parent,
                &tool_use_id,
                child_error_result(&tool_use_id, "sub-agent fan-out cap reached"),
            );
            continue;
        }

        // SPAWN: read the sub-agent prompt from the spawning `ToolUse` block (the
        // payload lives on the assistant turn recorded in `History`; the slot carries
        // only the `tool_use_id`).
        let prompt = world
            .entities
            .get(parent)
            .map(|e| subagent_prompt(&e.history, &tool_use_id))
            .unwrap_or_default();
        let (child_cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        let child_history = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text { text: prompt }],
        }]);
        // The child is a SUBAGENT in the same World, one level deeper, beginning its
        // own turn (`Thinking`) from the sub-agent prompt; `model: None` inherits the
        // world-default model.
        world.entities.insert(
            child,
            Components {
                identity: Identity::Subagent,
                lineage: Lineage {
                    parent: Some(*parent),
                    depth: child_depth,
                },
                history: child_history.clone(),
                activity: Activity::Thinking { cmd: child_cmd },
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
            },
        );
        // Bump the parent's CUMULATIVE spawn counter (never decremented — cumulative
        // semantics; design §1274). Also advance the local variable so a later slot in
        // the SAME batch sees the updated total against the cap.
        if let Some(parent_components) = world.entities.get_mut(parent) {
            parent_components.spawned = parent_components.spawned.saturating_add(1);
        }
        cumulative_spawned = cumulative_spawned.saturating_add(1);
        commands.push(Command::CallModel {
            cmd: child_cmd,
            entity: child,
            messages: child_history,
            tools: ToolSet::default(),
            params: default_model.clone(),
            key: CommandKey,
        });
    }

    // Share the all-slots-`Done` continuation with ToolSystem (fires now iff every
    // `Child` slot was denied and no other kind is still pending). `advance` re-checks
    // the gate and only emits for slots still `Pending { cmd: None }`, so calling it
    // after ToolSystem already ran this tick is idempotent.
    let (world, mut cont) = super::tool::advance(&world, parent);
    commands.append(&mut cont);
    (world, commands)
}

/// Settle a `ChildReturned` into the matching `Child` slot of `parent`: find the slot
/// whose `SlotKind::Child(c)` equals `child` AND whose `tool_use_id` matches (slot
/// identity is `(child, tool_use_id)`, NOT a `cmd` — Inv 16) and set it `Done`,
/// carrying the child's result re-stamped to the slot's `tool_use_id`. Returns the
/// settled World and whether a slot was actually resolved (so the caller advances only
/// when something changed).
fn settle_child_returned(
    world: &World,
    parent: &EntityId,
    child: EntityId,
    tool_use_id: &str,
    result: &Block,
) -> (World, bool) {
    let mut world = world.clone();
    let mut resolved = false;
    if let Some(e) = world.entities.get_mut(parent)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| {
            matches!(s.kind, SlotKind::Child(c) if c == child)
                && s.tool_use_id == tool_use_id
                && matches!(s.state, SlotState::Pending { .. })
        })
    {
        let tuid = slot.tool_use_id.clone();
        slot.result = Some(normalize_child_result(&tuid, result));
        slot.state = SlotState::Done;
        resolved = true;
    }
    (world, resolved)
}

/// Regenerate the `ChildReturned` inputs OWED by every child entity that has SETTLED
/// to `Idle` after its terminal `EndTurn` turn while its parent's owning `Child` slot
/// is still `Pending` — the COMPLETION half of in-process fan-out (the gap that, until
/// now, left a live parent waiting forever because nothing constructed `ChildReturned`).
///
/// This is the named, deliberately NON-fingerprinted exception to Inv 7
/// (docs/agent/world/ecs-runtime.md — `ChildReturned` regeneration; Correlation-pair
/// audit): the value is a PURE FUNCTION of `World` state at this tick — no model call,
/// no wall-clock, no RNG — so a faithful replay reproduces byte-identical inputs and a
/// counterfactual branch re-derives them WITHOUT a recorded/fingerprinted dispatch
/// boundary. The imperative shell-driver merely folds each through the existing
/// `settle_child_returned` path (correlated by `(child, tool_use_id)` identity, Inv 16),
/// so the parent resumes.
///
/// A child TERMINAL FAILURE is settled `is_error` by SupervisionSystem on the child's
/// `ModelFailed` in the SAME tick, so a failed/cancelled child's slot is already `Done`
/// here and is skipped; ONLY an `EndTurn`-settled child leaves its parent's slot
/// `Pending`, which is precisely the gap this fills. The `result` carries the child's
/// final answer (its last assistant message's blocks) as a `ToolResult`; `is_error:
/// false`.
///
/// Iterating the deterministically-ordered `entities` `BTreeMap` (never a `HashMap` —
/// Inv 8) keeps the owed-returns list free of a hidden ordering input.
pub(crate) fn regenerate_child_returns(world: &World) -> Vec<LogicalInput> {
    let mut returns = Vec::new();
    for (child_id, child) in &world.entities {
        // A child is owed a return only once it has SETTLED (`Idle`) and still has a
        // parent whose `Child` slot is awaiting it.
        if !matches!(child.activity, Activity::Idle) {
            continue;
        }
        let Some(parent_id) = child.lineage.parent else {
            continue;
        };
        let Some(parent) = world.entities.get(&parent_id) else {
            continue;
        };
        let Activity::ResolvingToolUses { slots } = &parent.activity else {
            continue;
        };
        for slot in slots {
            if matches!(slot.kind, SlotKind::Child(c) if c == *child_id)
                && matches!(slot.state, SlotState::Pending { .. })
            {
                returns.push(LogicalInput::ChildReturned {
                    parent: parent_id,
                    child: *child_id,
                    tool_use_id: slot.tool_use_id.clone(),
                    result: child_final_answer(&slot.tool_use_id, &child.history),
                });
            }
        }
    }
    returns
}

/// The child's final answer as the `ToolResult` its regenerated `ChildReturned` carries:
/// the content blocks of the child's LAST assistant message (the `EndTurn` turn TurnSystem
/// appended), stamped to the parent slot's `tool_use_id` so the assembled `tool_result`
/// pairs with its `tool_use`. Absent any assistant message the content is empty (Totality
/// — the parent still resumes). `is_error: false`: a failed child is owned by
/// SupervisionSystem, so this only ever runs for a child that produced an answer.
fn child_final_answer(tool_use_id: &str, history: &History) -> ToolResult {
    let content = history
        .0
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant))
        .map(|m| m.content.clone())
        .unwrap_or_default();
    Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content,
        is_error: false,
    }
}

/// Resolve the `Child` slot of `entity` whose `tool_use_id` matches: carry `result`
/// and set it `Done` — the depth-cap denial's inline resolution (Inv 10).
fn resolve_slot(world: &mut World, entity: &EntityId, tool_use_id: &str, result: ToolResult) {
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| s.tool_use_id == tool_use_id)
    {
        slot.result = Some(result);
        slot.state = SlotState::Done;
    }
}

/// Whether BOTH the App-wide `WorldGate` and `entity`'s `EntityGate` are Open — the
/// new-work gate guard a spawn must pass (Inv 13). A missing entity is treated as
/// closed (there is no parent to spawn into).
fn gates_open(world: &World, entity: &EntityId) -> bool {
    let entity_gate_open = world
        .entities
        .get(entity)
        .map(|e| e.gate.is_open())
        .unwrap_or(false);
    world.resources.gate.is_open() && entity_gate_open
}

/// Read the sub-agent prompt from the spawning `ToolUse` block, by its `tool_use_id`.
/// Prefers a `"prompt"` string field of the tool input; falls back to the whole input
/// serialised, else empty. The block lives on the assistant turn recorded in `History`.
fn subagent_prompt(history: &History, tool_use_id: &str) -> String {
    history
        .0
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            Block::ToolUse { id, input, .. } if id == tool_use_id => Some(
                input
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| input.to_string()),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// Build an `is_error` `ToolResult` for a denied / failed `Child` slot, stamped to the
/// slot's `tool_use_id` so the assembled `tool_result` pairs with its `tool_use`.
/// Shared with SupervisionSystem (the child terminal-failure case).
pub(super) fn child_error_result(tool_use_id: &str, message: &str) -> ToolResult {
    Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: vec![Block::Text {
            text: message.to_string(),
        }],
        is_error: true,
    }
}

/// Normalize a child's returned result into the `Block::ToolResult` a slot stores,
/// re-stamping its `tool_use_id` to the slot's so the assembled `tool_result` pairs
/// with its `tool_use`. A child failure rides as `is_error: true` (Inv 10).
fn normalize_child_result(tool_use_id: &str, result: &Block) -> ToolResult {
    match result {
        Block::ToolResult {
            content, is_error, ..
        } => Block::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.clone(),
            is_error: *is_error,
        },
        other => Block::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: vec![other.clone()],
            is_error: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::world::effects::Command;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{Block, Role, ToolResult};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, EntityId, Identity, Lineage, ModelConfig, Resources,
        SlotKind, SlotState, Tick, ToolUseId, World,
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

    /// A `ModelResponded·ToolUse` carrying one `spawn_subagent` block (the sub-agent
    /// prompt rides in its input), addressed to the primary entity `0`.
    fn spawn_subagent_response(cmd: CmdId, tool_use_id: &str, prompt: &str, at: Tick) -> Event {
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
                    input: serde_json::json!({ "prompt": prompt }),
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

    fn child_returned(
        parent: EntityId,
        child: EntityId,
        tool_use_id: &str,
        is_error: bool,
        at: Tick,
    ) -> Event {
        let result: ToolResult = Block::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: vec![Block::Text {
                text: "child answer".into(),
            }],
            is_error,
        };
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ChildReturned {
                parent,
                child,
                tool_use_id: tool_use_id.into(),
                result,
            },
        }
    }

    fn turn_cmd_of(commands: &[Command]) -> CmdId {
        match commands.first() {
            Some(Command::CallModel { cmd, .. }) => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    /// The single `Child` slot of entity `0`'s `ResolvingToolUses` turn, panicking
    /// otherwise. Returns `(child_entity_id, tool_use_id, state)`.
    fn child_slot(world: &World) -> (EntityId, ToolUseId, SlotState) {
        let slots = match &world.entities.get(&0).expect("parent").activity {
            Activity::ResolvingToolUses { slots } => slots.clone(),
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        assert_eq!(slots.len(), 1, "exactly one slot");
        let child = match slots[0].kind {
            SlotKind::Child(c) => c,
            other => panic!("expected a Child slot, got {other:?}"),
        };
        (child, slots[0].tool_use_id.clone(), slots[0].state)
    }

    /// Drive a primary entity to a `spawn_subagent` `Child` slot. Returns the World,
    /// the child entity id, and its `tool_use_id`.
    fn world_with_spawned_child() -> (World, EntityId, ToolUseId) {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, _commands) =
            tick(&world, &spawn_subagent_response(turn_cmd, "tu_child", "do x", 2));
        let (child, tuid, _state) = child_slot(&world);
        (world, child, tuid)
    }

    /// [VC-1.4] A `spawn_subagent` ToolUse branches into a `Child` slot
    /// (`Pending { cmd: None }`, Inv 16) AND spawns a child Entity in `world.entities`
    /// with `lineage = parent`, `depth = parent.depth + 1`, beginning its own turn.
    #[test]
    fn spawn_subagent_creates_child_slot_and_spawns_child_entity() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);

        let (world, commands) =
            tick(&world, &spawn_subagent_response(turn_cmd, "tu_child", "research X", 2));

        // The parent's slot is a `Child`, still `Pending { cmd: None }` (a child has no
        // shell-dispatched command — Inv 16).
        let (child, tuid, state) = child_slot(&world);
        assert_eq!(tuid, "tu_child");
        assert_eq!(state, SlotState::Pending { cmd: None }, "Child slot is cmd: None (Inv 16)");
        assert_ne!(child, 0, "the child id is distinct from the root");

        // The child Entity exists in the SAME World: a Subagent, lineage = parent,
        // depth + 1, beginning its own turn from the sub-agent prompt.
        let c = world.entities.get(&child).expect("child spawned into world.entities");
        assert_eq!(c.identity, Identity::Subagent);
        assert_eq!(c.lineage, Lineage { parent: Some(0), depth: 1 });
        let child_cmd = match c.activity {
            Activity::Thinking { cmd } => cmd,
            ref other => panic!("child begins its own turn (Thinking), got {other:?}"),
        };
        assert!(
            matches!(
                &c.history.0[0],
                crate::agent::world::history::Msg { role: Role::User, content }
                    if matches!(content.as_slice(), [Block::Text { text }] if text == "research X")
            ),
            "the child is seeded with the sub-agent prompt"
        );

        // The child's own turn was dispatched: exactly one `CallModel`, routed to the
        // child on its `Thinking` cmd.
        let child_calls: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { entity, .. } if *entity == child))
            .collect();
        assert_eq!(child_calls.len(), 1, "the child's turn emits one CallModel");
        match child_calls[0] {
            Command::CallModel { cmd, .. } => assert_eq!(*cmd, child_cmd),
            _ => unreachable!(),
        }
    }

    /// [VC-1.4] `ChildReturned { parent, child, tool_use_id, result }` resolves the
    /// parent's `Child` slot by IDENTITY `(child, tool_use_id)` → `Done`; with the slot
    /// the only one, the shared `tool::advance` continuation fires (`→ Thinking`, ONE
    /// user Msg of the result, one `CallModel`).
    #[test]
    fn child_returned_resolves_slot_by_identity_and_continues() {
        let (world, child, tuid) = world_with_spawned_child();

        let (world, commands) = tick(&world, &child_returned(0, child, &tuid, false, 3));

        // The parent advanced to a fresh turn: the all-slots-Done continuation fired.
        let e = world.entities.get(&0).expect("parent");
        let cont_cmd = match e.activity {
            Activity::Thinking { cmd } => cmd,
            ref other => panic!("expected Thinking continuation, got {other:?}"),
        };
        let calls: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { entity: 0, .. }))
            .collect();
        assert_eq!(calls.len(), 1, "exactly one continuation CallModel for the parent");
        match calls[0] {
            Command::CallModel { cmd, .. } => assert_eq!(*cmd, cont_cmd),
            _ => unreachable!(),
        }

        // History: user + assistant(tool_use) + ONE user(tool_result) — the child's
        // result rides in the assembled `tool_result`, paired with its `tool_use_id`.
        assert_eq!(e.history.0.len(), 3);
        let last = &e.history.0[2];
        assert!(matches!(last.role, Role::User));
        match &last.content[0] {
            Block::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, &tuid, "result paired with the Child slot's tool_use_id");
                assert!(!is_error, "a successful child result is not an error");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// [VC-1.4, Inv 16] A `ChildReturned` whose `child` does NOT match the slot's
    /// `SlotKind::Child(_)` does not resolve the slot — correlation is by IDENTITY
    /// `(child, tool_use_id)`, not by `tool_use_id` alone.
    #[test]
    fn child_returned_with_a_mismatched_child_does_not_resolve() {
        let (world, child, tuid) = world_with_spawned_child();
        let wrong_child = child + 999;

        let (world, commands) = tick(&world, &child_returned(0, wrong_child, &tuid, false, 3));

        // No continuation: the slot is still Pending, the parent still resolving.
        assert!(
            !commands.iter().any(|c| matches!(c, Command::CallModel { entity: 0, .. })),
            "a mismatched child must not fire the continuation"
        );
        let (_child, _tuid, state) = child_slot(&world);
        assert_eq!(state, SlotState::Pending { cmd: None }, "the slot stays Pending");
    }

    /// [VC-1.4, Inv 10] A depth-cap denial (`child depth > depth_cap`) resolves the
    /// `Child` slot `is_error` WITHOUT spawning a child Entity; being the only slot,
    /// the continuation fires immediately so the parent is never stuck.
    #[test]
    fn depth_cap_denial_resolves_is_error_without_spawning() {
        let mut world = idle_world();
        // A child of the root would be depth 1; cap 0 denies it.
        world.resources.depth_cap = 0;
        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);

        let (world, commands) =
            tick(&world, &spawn_subagent_response(turn_cmd, "tu_child", "do x", 2));

        // NO child Entity was spawned — only the root remains.
        assert_eq!(world.entities.len(), 1, "a denied spawn creates no child Entity");
        assert!(
            !commands.iter().any(|c| matches!(c, Command::CallModel { entity, .. } if *entity != 0)),
            "a denied spawn emits no child CallModel"
        );

        // The slot resolved `is_error` and the continuation fired (parent → Thinking).
        let e = world.entities.get(&0).expect("parent");
        assert!(matches!(e.activity, Activity::Thinking { .. }), "all slots Done → continuation");
        match &e.history.0[2].content[0] {
            Block::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, "tu_child");
                assert!(*is_error, "a depth-capped spawn resolves is_error (Inv 10)");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// A child of the root (depth 1) with `depth_cap = 1` is allowed — the boundary
    /// is `child depth > cap`, not `>=`.
    #[test]
    fn depth_cap_allows_a_child_at_the_cap() {
        let mut world = idle_world();
        world.resources.depth_cap = 1;
        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, _commands) =
            tick(&world, &spawn_subagent_response(turn_cmd, "tu_child", "do x", 2));

        let (child, _tuid, _state) = child_slot(&world);
        assert!(world.entities.contains_key(&child), "depth == cap is allowed (deny is strictly >)");
    }

    /// A `ModelResponded·ToolUse` asking for TWO sub-agents at once — two `Child`
    /// slots in one assistant turn, so SubagentSystem decides both in one batch.
    fn spawn_two_subagents_response(cmd: CmdId, tu_a: &str, tu_b: &str, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![
                    Block::ToolUse {
                        id: tu_a.into(),
                        name: "spawn_subagent".into(),
                        input: serde_json::json!({ "prompt": "a" }),
                    },
                    Block::ToolUse {
                        id: tu_b.into(),
                        name: "spawn_subagent".into(),
                        input: serde_json::json!({ "prompt": "b" }),
                    },
                ],
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

    /// [VC-2.3, Inv 10/11] With `fanout_cap = 1`, a turn requesting TWO sub-agents
    /// spawns the FIRST and DENIES the second: the denied `Child` slot resolves
    /// `is_error` WITHOUT inserting a child Entity (so `world.entities` does not grow
    /// past parent + one child), mirroring the depth-cap deny.
    #[test]
    fn fanout_cap_denies_spawn_past_live_child_cap_without_growing_entities() {
        let mut world = idle_world();
        // The primary may hold at most ONE live child.
        world.entities.get_mut(&0).expect("primary").budget.limits.fanout_cap = 1;

        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);

        let (world, commands) =
            tick(&world, &spawn_two_subagents_response(turn_cmd, "tu_a", "tu_b", 2));

        // Only ONE child Entity exists (parent + one child) — the 2nd spawn is denied.
        assert_eq!(
            world.entities.len(),
            2,
            "the fan-out cap denies the 2nd spawn — no 2nd child Entity is inserted"
        );
        // Exactly one child CallModel was dispatched (the admitted child's turn).
        let child_calls = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { entity, .. } if *entity != 0))
            .count();
        assert_eq!(child_calls, 1, "only the admitted child's turn is dispatched");

        let slots = match &world.entities.get(&0).expect("parent").activity {
            Activity::ResolvingToolUses { slots } => slots.clone(),
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        assert_eq!(slots.len(), 2, "both tool_use blocks became slots");

        // tu_a: admitted — the child exists and the slot is still Pending (awaiting return).
        let slot_a = slots.iter().find(|s| s.tool_use_id == "tu_a").expect("slot a");
        assert_eq!(slot_a.state, SlotState::Pending { cmd: None }, "the admitted spawn stays Pending");
        let child_a = match slot_a.kind {
            SlotKind::Child(c) => c,
            other => panic!("expected a Child slot, got {other:?}"),
        };
        assert!(world.entities.contains_key(&child_a), "the admitted child exists in the World");

        // tu_b: denied — settled `is_error`, NO child Entity created.
        let slot_b = slots.iter().find(|s| s.tool_use_id == "tu_b").expect("slot b");
        assert_eq!(slot_b.state, SlotState::Done, "the denied spawn is settled, not left pending");
        let child_b = match slot_b.kind {
            SlotKind::Child(c) => c,
            other => panic!("expected a Child slot, got {other:?}"),
        };
        assert!(!world.entities.contains_key(&child_b), "the denied spawn created NO child Entity");
        match &slot_b.result {
            Some(Block::ToolResult { is_error, tool_use_id, .. }) => {
                assert!(*is_error, "a fan-out-capped spawn resolves is_error (Inv 10)");
                assert_eq!(tool_use_id, "tu_b");
            }
            other => panic!("expected an is_error ToolResult, got {other:?}"),
        }
    }

    /// A `fanout_cap` of 0 is UNBOUNDED: a turn requesting two sub-agents spawns BOTH
    /// (parent + two children), so the brake is off (mirrors the `0 ⇒ unbounded`
    /// convention of every cap).
    #[test]
    fn fanout_cap_zero_is_unbounded_spawns_all() {
        let world = idle_world(); // default fanout_cap = 0 (unbounded)
        let (world, commands) = tick(&world, &user_message("go", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, _commands) =
            tick(&world, &spawn_two_subagents_response(turn_cmd, "tu_a", "tu_b", 2));
        assert_eq!(
            world.entities.len(),
            3,
            "fanout_cap 0 is unbounded — both spawns are admitted (parent + two children)"
        );
    }
}
