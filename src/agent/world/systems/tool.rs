//! tool — the ToolSystem: owns the `Local` slot kind of a `ResolvingToolUses`
//! turn. See docs/agent/world/ecs-runtime.md (ToolSystem; Tick discipline phase 5).
//!
//! ToolSystem has two roles over the tool-resolution loop, both driven purely from
//! `(World, Input)`:
//!
//! - **Emit** — for each `Local` `ToolSlot` still `Pending { cmd: None }` it mints
//!   a `cmd` and emits one `RunTool`, recording that `cmd` in the slot's
//!   `Pending { cmd: Some(cmd) }`. The slot's identity is its `cmd` (Inv 16). This
//!   fires the tick a `ToolUse` turn enters `ResolvingToolUses` (TurnSystem ran in
//!   phase 3 the same tick).
//! - **Settle + advance** — on `ToolReturned` it records the tool output into the
//!   `Local` slot whose `Pending` `cmd` matches and sets it `Done` (a tool ERROR
//!   rides as `is_error: true` in that `ToolResult`, Inv 10). Once EVERY slot of
//!   the turn is `Done` — across ALL kinds, not just `Local` — it appends EXACTLY
//!   ONE user `Msg` holding every slot's `ToolResult` in ASCENDING `ordinal` order,
//!   transitions `→ Thinking`, and emits the continuation `CallModel`.
//!
//! P1a-tool owns only `Local`. Non-`Local` slots (Child/Human/Peer) are created by
//! TurnSystem but left untouched here; their owning Systems resolve them, and the
//! all-`Done` gate above keeps the turn waiting until they do.

use super::{Input, System};
use crate::agent::world::effects::{Command, CommandKey, ToolName, ToolSet};
use crate::agent::world::history::{Block, History, Json, Msg, Role, ToolResult};
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{
    Activity, CmdId, EntityId, SlotKind, SlotState, ToolUseId, World,
};

/// ToolSystem — drives a `ResolvingToolUses` turn's `Local` slots (phase 5 of the
/// tick): emit `RunTool` per pending slot, settle each `ToolReturned`, and continue
/// once every slot is `Done`.
pub struct ToolSystem;

impl System for ToolSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // Settle the tool result into its slot (matched by `cmd`), then advance.
            LogicalInput::ToolReturned {
                cmd, entity, result, ..
            } => {
                let world = settle_tool_returned(world, entity, *cmd, result);
                advance(&world, entity)
            }
            // A `ToolUse` turn just entered `ResolvingToolUses` this same tick
            // (TurnSystem, phase 3): emit the `RunTool` for each fresh `Local` slot.
            LogicalInput::ModelResponded { entity, .. } => advance(world, entity),
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// Fold a `ToolReturned` into the matching `Local` slot: find the slot whose
/// `Pending { cmd: Some(c) }` equals the returned `cmd` (slot identity is the
/// `cmd`, NOT arrival order — Inv 16) and set it `Done`, carrying the tool output
/// as a `Block::ToolResult`. A tool ERROR rides as `is_error: true` in that same
/// `ToolResult` (Inv 10), so the model sees a well-formed `tool_result` next turn.
fn settle_tool_returned(world: &World, entity: &EntityId, cmd: CmdId, result: &[Block]) -> World {
    let mut world = world.clone();
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| {
            matches!(s.kind, SlotKind::Local)
                && matches!(s.state, SlotState::Pending { cmd: Some(c) } if c == cmd)
        })
    {
        let tool_use_id = slot.tool_use_id.clone();
        slot.result = Some(tool_result_block(&tool_use_id, result));
        slot.state = SlotState::Done;
    }
    world
}

/// Advance the entity's `ResolvingToolUses` turn: emit a `RunTool` for every
/// `Local` slot still `Pending { cmd: None }` (minting and recording its `cmd`),
/// and — once EVERY slot is `Done` — assemble the continuation (one user `Msg` of
/// all results in ascending `ordinal` order, `→ Thinking`, emit `CallModel`). A
/// no-op for any entity not in `ResolvingToolUses`.
///
/// Visible to sibling Systems (HumanActionSystem, InteractionSystem) so they can
/// share the all-slots-`Done` continuation gate without duplicating it (Inv 10).
pub(super) fn advance(world: &World, entity: &EntityId) -> (World, Vec<Command>) {
    // Gate guard (Invariant 13): advancing a turn is NEW WORK — emitting a slot's
    // `RunTool` or assembling the continuation `CallModel` — so it requires BOTH the
    // App-wide `WorldGate` and this entity's `EntityGate` Open. A closed gate DEFERS
    // the advance; the entity waits at its boundary until the gate reopens. The
    // already-landed result was settled BEFORE this (`settle_tool_returned`, phase 3),
    // which is gate-proof. See docs/agent/world/ecs-runtime.md (GateSystem; ToolSystem).
    let entity_gate_open = world
        .entities
        .get(entity)
        .map(|e| e.gate.is_open())
        .unwrap_or(true);
    if !world.resources.gate.is_open() || !entity_gate_open {
        return (world.clone(), Vec::new());
    }

    let mut world = world.clone();
    let mut commands = Vec::new();

    // Snapshot the pending `Local` work (tool_use_id + name + input) before minting,
    // so the read-borrow of `world` is released before the mint/mutate below. The
    // tool name/input live on the assistant turn's `ToolUse` block in History (the
    // slot stores only the `tool_use_id`).
    let pending: Vec<(ToolUseId, ToolName, Json)> = match world.entities.get(entity) {
        Some(e) => match &e.activity {
            Activity::ResolvingToolUses { slots } => slots
                .iter()
                .filter(|s| matches!(s.kind, SlotKind::Local))
                .filter(|s| matches!(s.state, SlotState::Pending { cmd: None }))
                .filter_map(|s| {
                    find_tool_use(&e.history, &s.tool_use_id)
                        .map(|(name, input)| (s.tool_use_id.clone(), name.to_string(), input.clone()))
                })
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };

    // Mint a `cmd` per pending slot, emit its `RunTool`, and record the `cmd` so the
    // matching `ToolReturned` resolves it (Inv 16). Mint order follows slot order
    // (ascending `ordinal`), so replay mints byte-identical ids (Inv 8).
    for (tool_use_id, tool, args) in pending {
        let (cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        commands.push(Command::RunTool {
            cmd,
            entity: *entity,
            tool,
            args,
            key: CommandKey,
        });
        if let Some(e) = world.entities.get_mut(entity)
            && let Activity::ResolvingToolUses { slots } = &mut e.activity
            && let Some(slot) = slots.iter_mut().find(|s| s.tool_use_id == tool_use_id)
        {
            slot.state = SlotState::Pending { cmd: Some(cmd) };
        }
    }

    // Continue only once EVERY slot of the turn is `Done` (across ALL kinds — a
    // non-`Local` slot still `Pending` keeps the turn waiting for its own System).
    let all_done = match world.entities.get(entity) {
        Some(e) => match &e.activity {
            Activity::ResolvingToolUses { slots } => {
                !slots.is_empty() && slots.iter().all(|s| matches!(s.state, SlotState::Done))
            }
            _ => false,
        },
        None => false,
    };
    if all_done {
        // Mint the continuation `cmd` (separate borrow from the entity mutation
        // below; replay mints identically — Inv 8), as IntakeSystem does.
        let (cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        let default_model = world.resources.model.clone();
        // Offer the App's ROOT (surface-capable) entity its surface tools on this
        // tool-loop continuation, so the model can request `set_value` again mid-turn
        // (GAP A). A non-root entity (a sub-agent) is offered none. See
        // docs/agent/world/ecs-runtime.md (Anthropic model-call mapping).
        let surface_tools = if *entity == world.root {
            world.resources.surface_tools.clone()
        } else {
            ToolSet::default()
        };
        if let Some(e) = world.entities.get_mut(entity) {
            // Assemble the single user Msg: every slot's `ToolResult` in ASCENDING
            // `ordinal` order (the assistant block order — NOT arrival order).
            let mut results: Vec<(u16, ToolResult)> = match &e.activity {
                Activity::ResolvingToolUses { slots } => slots
                    .iter()
                    .filter_map(|s| s.result.clone().map(|r| (s.ordinal, r)))
                    .collect(),
                _ => Vec::new(),
            };
            results.sort_by_key(|(ordinal, _)| *ordinal);
            let content: Vec<Block> = results.into_iter().map(|(_, result)| result).collect();
            e.history.0.push(Msg {
                role: Role::User,
                content,
            });
            e.activity = Activity::Thinking { cmd };
            let params = e.model.clone().unwrap_or(default_model);
            let messages = e.history.clone();
            commands.push(Command::CallModel {
                cmd,
                entity: *entity,
                messages,
                tools: surface_tools,
                params,
                key: CommandKey,
            });
        }
    }

    (world, commands)
}

/// Look up the `(name, input)` of the `ToolUse` block a slot backs, by its
/// `tool_use_id` (unique per the Anthropic contract). The slot carries only the
/// id; the dispatch payload lives on the assistant turn recorded in `History`.
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

/// Normalize a `ToolReturned` payload into the `Block::ToolResult` a slot stores.
/// The result blocks are the tool's output: when the tool errored its driver hands
/// back a pre-formed `Block::ToolResult { is_error: true, .. }`, adopted here (its
/// `tool_use_id` re-stamped to the slot's so the assembled `tool_result` pairs with
/// its `tool_use`); otherwise the blocks become the `content` of a fresh, non-error
/// `ToolResult`. See docs/agent/world/ecs-runtime.md (`ToolReturned` — "a tool
/// ERROR rides as `is_error` in the ToolResult").
fn tool_result_block(tool_use_id: &str, result: &[Block]) -> ToolResult {
    if let [Block::ToolResult {
        content, is_error, ..
    }] = result
    {
        Block::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.clone(),
            is_error: *is_error,
        }
    } else {
        Block::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: result.to_vec(),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::world::effects::Command;
    use crate::agent::world::history::{Block, History, Json, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::gates::{EntityGate, PauseReason};
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, SlotKind,
        SlotState, Tick, World,
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

    /// A `ModelResponded` whose `stop_reason == ToolUse`, carrying the given
    /// `(id, name, input)` tool_use blocks in order.
    fn model_responded_tool_use(cmd: CmdId, tool_uses: &[(&str, &str, Json)], at: Tick) -> Event {
        let blocks = tool_uses
            .iter()
            .map(|(id, name, input)| Block::ToolUse {
                id: (*id).into(),
                name: (*name).into(),
                input: input.clone(),
            })
            .collect();
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks,
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

    fn pause(at: Tick) -> Event {
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

    fn tool_returned(cmd: CmdId, result: Vec<Block>, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ToolReturned {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                result,
            },
        }
    }

    /// The `(cmd, tool)` of every emitted `RunTool` in `commands`.
    fn run_tools(commands: &[Command]) -> Vec<(CmdId, String)> {
        commands
            .iter()
            .filter_map(|c| match c {
                Command::RunTool { cmd, tool, .. } => Some((*cmd, tool.clone())),
                _ => None,
            })
            .collect()
    }

    fn turn_cmd_of(commands: &[Command]) -> CmdId {
        match commands.first() {
            Some(Command::CallModel { cmd, .. }) => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    /// VC-1.1: a `ModelResponded·ToolUse` with two tool_use blocks branches into
    /// `ResolvingToolUses` with two ordinal-tagged `Local` slots, and ToolSystem
    /// emits exactly one `RunTool` per slot, each resolvable by its minted `cmd`.
    #[test]
    fn tool_use_turn_creates_local_slots_and_emits_one_run_tool_each() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("hi", 1));
        let turn_cmd = turn_cmd_of(&commands);

        let (world, commands) = tick(
            &world,
            &model_responded_tool_use(
                turn_cmd,
                &[
                    ("tu_a", "tool_a", serde_json::json!({ "x": 1 })),
                    ("tu_b", "tool_b", serde_json::json!({ "y": 2 })),
                ],
                2,
            ),
        );

        let e = world.entities.get(&0).expect("entity");
        let slots = match &e.activity {
            Activity::ResolvingToolUses { slots } => slots,
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        };
        // Two Local slots, in ordinal (assistant-block) order.
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].tool_use_id.as_str(), slots[0].ordinal), ("tu_a", 0));
        assert_eq!((slots[1].tool_use_id.as_str(), slots[1].ordinal), ("tu_b", 1));
        assert!(slots.iter().all(|s| matches!(s.kind, SlotKind::Local)));
        // Each slot now carries the minted cmd that resolves it (Inv 16).
        assert!(slots
            .iter()
            .all(|s| matches!(s.state, SlotState::Pending { cmd: Some(_) })));

        // The assistant turn was recorded: user + assistant.
        assert_eq!(e.history.0.len(), 2);

        // Exactly one RunTool per Local slot; each slot's recorded cmd has one.
        let runs = run_tools(&commands);
        assert_eq!(runs.len(), 2, "one RunTool per Local slot");
        assert_eq!(commands.len(), 2, "no other commands this tick");
        for slot in slots {
            let SlotState::Pending { cmd: Some(c) } = slot.state else {
                panic!("slot cmd must be minted");
            };
            assert!(
                runs.iter().any(|(rc, _)| *rc == c),
                "a RunTool must carry slot cmd {c}"
            );
        }
    }

    /// VC-1.1: results matched by `cmd` (NOT arrival order); only once EVERY slot
    /// is `Done` is ONE user `Msg` assembled with results in ASCENDING `ordinal`
    /// order, the entity goes `→ Thinking`, and exactly one `CallModel` is emitted.
    #[test]
    fn all_slots_done_assembles_one_user_msg_in_ordinal_order_and_continues() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("hi", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, commands) = tick(
            &world,
            &model_responded_tool_use(
                turn_cmd,
                &[
                    ("tu_a", "tool_a", serde_json::json!({})),
                    ("tu_b", "tool_b", serde_json::json!({})),
                ],
                2,
            ),
        );
        let runs = run_tools(&commands);
        let cmd_a = runs.iter().find(|(_, t)| t == "tool_a").expect("cmd_a").0;
        let cmd_b = runs.iter().find(|(_, t)| t == "tool_b").expect("cmd_b").0;

        // Resolve tu_b FIRST (arrival ≠ ordinal). Not all done → no continuation.
        let (world, commands) =
            tick(&world, &tool_returned(cmd_b, vec![Block::Text { text: "B".into() }], 3));
        assert!(commands.is_empty(), "partial resolution emits no continuation");
        let slots = match &world.entities.get(&0).expect("entity").activity {
            Activity::ResolvingToolUses { slots } => slots.clone(),
            other => panic!("still ResolvingToolUses, got {other:?}"),
        };
        assert!(matches!(slots[1].state, SlotState::Done), "tu_b settled");
        assert!(
            matches!(slots[0].state, SlotState::Pending { .. }),
            "tu_a still pending"
        );

        // Resolve tu_a → all Done → continuation.
        let (world, commands) =
            tick(&world, &tool_returned(cmd_a, vec![Block::Text { text: "A".into() }], 4));

        let e = world.entities.get(&0).expect("entity");
        let cont_cmd = match &e.activity {
            Activity::Thinking { cmd } => *cmd,
            other => panic!("expected Thinking, got {other:?}"),
        };
        let call_models: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { .. }))
            .collect();
        assert_eq!(call_models.len(), 1, "exactly one continuation CallModel");
        let (call_cmd, messages) = match call_models[0] {
            Command::CallModel { cmd, messages, .. } => (*cmd, messages),
            _ => unreachable!(),
        };
        assert_eq!(call_cmd, cont_cmd, "CallModel carries the new turn cmd");

        // History: user + assistant(tool_use) + ONE user(tool_result) message.
        assert_eq!(e.history.0.len(), 3);
        let last = &e.history.0[2];
        assert!(matches!(last.role, Role::User), "results ride in a user Msg");
        let ids: Vec<&str> = last
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        // ASCENDING ordinal order (tu_a then tu_b), NOT arrival order (tu_b first).
        assert_eq!(ids, vec!["tu_a", "tu_b"], "results assembled in ordinal order");
        assert_eq!(messages.0.len(), 3, "continuation carries the assembled history");
    }

    /// VC-1.2 (continuation aspect): while PAUSED, a `ToolReturned` that completes
    /// the last slot STILL settles that slot (gate-proof result-settling, Inv 13),
    /// but the turn-advance continuation `CallModel` is DEFERRED — the entity waits
    /// at the all-slots-`Done` boundary in `ResolvingToolUses` for the gate to reopen.
    #[test]
    fn paused_defers_continuation_but_settles_the_slot() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("hi", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, commands) = tick(
            &world,
            &model_responded_tool_use(turn_cmd, &[("tu_x", "tool_x", serde_json::json!({}))], 2),
        );
        let runs = run_tools(&commands);
        assert_eq!(runs.len(), 1);
        let cmd_x = runs[0].0;

        // Pause mid-resolution.
        let (world, _) = tick(&world, &pause(3));
        assert!(!world.resources.gate.is_open(), "gate closed mid-resolve");

        // The single slot's result lands while paused: it SETTLES (slot → Done) but
        // the continuation is DEFERRED — no CallModel, entity still ResolvingToolUses.
        let (world, commands) =
            tick(&world, &tool_returned(cmd_x, vec![Block::Text { text: "X".into() }], 4));
        assert!(
            !commands.iter().any(|c| matches!(c, Command::CallModel { .. })),
            "the continuation is deferred while paused"
        );
        let e = world.entities.get(&0).expect("entity");
        let slots = match &e.activity {
            Activity::ResolvingToolUses { slots } => slots,
            other => panic!("entity waits in ResolvingToolUses, got {other:?}"),
        };
        assert!(
            matches!(slots[0].state, SlotState::Done),
            "the in-flight result still settled into its slot (Inv 13)"
        );
    }

    /// Inv 10: a tool error result rides as an `is_error` `ToolResult`.
    #[test]
    fn tool_error_result_rides_as_is_error() {
        let world = idle_world();
        let (world, commands) = tick(&world, &user_message("hi", 1));
        let turn_cmd = turn_cmd_of(&commands);
        let (world, commands) = tick(
            &world,
            &model_responded_tool_use(turn_cmd, &[("tu_x", "tool_x", serde_json::json!({}))], 2),
        );
        let runs = run_tools(&commands);
        assert_eq!(runs.len(), 1);
        let cmd_x = runs[0].0;

        // The tool errored: its driver hands back a pre-formed is_error ToolResult.
        let (world, commands) = tick(
            &world,
            &tool_returned(
                cmd_x,
                vec![Block::ToolResult {
                    tool_use_id: "tu_x".into(),
                    content: vec![Block::Text { text: "boom".into() }],
                    is_error: true,
                }],
                3,
            ),
        );

        // Single slot done → one continuation; the assembled tool_result is_error.
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, Command::CallModel { .. }))
                .count(),
            1
        );
        let e = world.entities.get(&0).expect("entity");
        assert!(matches!(e.activity, Activity::Thinking { .. }));
        match &e.history.0[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "tu_x");
                assert!(*is_error, "a tool error rides as is_error (Inv 10)");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }
}
