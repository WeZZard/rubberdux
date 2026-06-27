//! intake — the IntakeSystem (phase 2). See docs/agent/world/ecs-runtime.md.
//!
//! Intake admits a `UserMessage` and STARTS a turn: when the addressed entity is
//! `Idle`, it DRAINS any messages SteeringSystem parked on the entity's `Inbox`
//! while it was mid-run (FIFO, honoured BEFORE the triggering message), pushes the
//! triggering user `Msg` into `History`, mints the turn's `cmd` via `Resources.ids`,
//! transitions `Idle → Thinking { cmd }`, and emits the `CallModel`. "Admit" is
//! "the entity is Idle and both gates Open"; a mid-run `UserMessage` is enqueued by
//! SteeringSystem (it does not initiate here) and merged on this next initiation.
//!
//! Loop guard (Boundedness, Inv 11): each initiation increments the per-entity turn
//! counter (`Components.turns`); on reaching `Budget.limits.loop_cap` (cap != 0) the
//! initiation becomes a FINAL wrap-up turn — a wrap-up directive is injected so the
//! model concludes — after which Intake STOPS initiating turns for that entity. The
//! counter is World state, so the brake replays deterministically.

use super::{Input, System};
use crate::agent::world::effects::{Command, CommandKey, ToolSet};
use crate::agent::world::history::{Block, Msg, Role};
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, World};

/// The wrap-up directive IntakeSystem injects on the FINAL turn once the loop cap is
/// reached: a user-role instruction telling the model to conclude without looping
/// further (tools are already withheld on this turn). Reuses Pass 3's "step budget
/// exhausted; finalize now" recovery phrasing. See docs/agent/world/ecs-runtime.md
/// (Loop guard — bound the tool/turn loop).
const LOOP_CAP_WRAP_UP: &str = "step budget exhausted; finalize now";

/// IntakeSystem — admits a `UserMessage` into a fresh turn (phase 2 of the tick).
pub struct IntakeSystem;

impl System for IntakeSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        let LogicalInput::UserMessage { to, text } = input else {
            return (world.clone(), Vec::new());
        };
        // P0 admits a UserMessage only into an Idle entity (start a turn). A
        // mid-run message (entity Thinking) is enqueued/steered in P1a, not here.
        let idle = world
            .entities
            .get(to)
            .map(|e| matches!(e.activity, Activity::Idle))
            .unwrap_or(false);
        if !idle {
            return (world.clone(), Vec::new());
        }

        // Gate guard (Invariant 13): INITIATING a turn requires BOTH the App-wide
        // `WorldGate` and the addressed entity's `EntityGate` to be Open. A closed
        // gate DEFERS admission — the entity stays Idle and the message is not
        // consumed — it is never swallowed; result-settling (phase 3) is gate-proof.
        // See docs/agent/world/ecs-runtime.md (GateSystem; IntakeSystem).
        let entity_gate_open = world
            .entities
            .get(to)
            .map(|e| e.gate.is_open())
            .unwrap_or(false);
        if !world.resources.gate.is_open() || !entity_gate_open {
            return (world.clone(), Vec::new());
        }

        // Loop guard (Boundedness, Inv 11): the per-entity turn counter is World state
        // so the brake replays deterministically. Read it and the entity's `loop_cap`
        // BEFORE mutating. A `loop_cap` of 0 is unbounded (no brake).
        let (turns, loop_cap) = match world.entities.get(to) {
            Some(e) => (e.turns, e.budget.limits.loop_cap),
            None => return (world.clone(), Vec::new()),
        };
        // Already looped-out: the final wrap-up turn was forced on a PRIOR initiation
        // (turns reached the cap), so STOP — initiate no further turn. The counter is
        // the marker; nothing re-arms the loop after the wrap-up.
        if loop_cap != 0 && turns >= loop_cap {
            return (world.clone(), Vec::new());
        }
        // Initiating this turn advances the counter; reaching the cap on THIS turn makes
        // it the FINAL wrap-up turn (a directive is injected below so the model concludes
        // instead of looping). `saturating_add` keeps the counter total.
        let next_turns = turns.saturating_add(1);
        let final_wrap_up = loop_cap != 0 && next_turns >= loop_cap;

        let mut world = world.clone();
        // Mint the turn's correlation `cmd` from the SOLE id minter (Resources.ids),
        // as a pure function of World state, so replay mints the same id (Inv 8).
        let (cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        let default_model = world.resources.model.clone();
        // The surface tools offered to the App's ROOT (surface-capable) entity, read
        // into this turn's `ToolSet` so a live model call is told `set_value` EXISTS
        // and can request it. A non-root entity (a sub-agent) is offered none, and the
        // FINAL wrap-up turn withholds tools (below) so the run cannot re-arm the loop.
        // See docs/agent/world/ecs-runtime.md (Anthropic model-call mapping; Loop guard).
        let surface_tools = if *to == world.root {
            world.resources.surface_tools.clone()
        } else {
            ToolSet::default()
        };

        // Re-borrow the entity from the clone (presence + Idle confirmed above).
        let Some(entity) = world.entities.get_mut(to) else {
            return (world, Vec::new());
        };
        // Drain any messages SteeringSystem parked on the Inbox while this entity
        // was mid-run, honouring them (FIFO — oldest first) as user input for this
        // turn BEFORE the triggering message, then leave the Inbox empty. A steered
        // message is older than the message initiating this turn, so it precedes it
        // in History. See docs/agent/world/ecs-runtime.md (Inbox; SteeringSystem).
        for content in std::mem::take(&mut entity.inbox.pending) {
            entity.history.0.push(Msg {
                role: Role::User,
                content,
            });
        }
        entity.history.0.push(Msg {
            role: Role::User,
            content: vec![Block::Text { text: text.clone() }],
        });
        // FINAL wrap-up turn: inject a wrap-up directive (last, so it is the most
        // salient instruction) telling the model to conclude. Tools are already
        // withheld here (Intake's `CallModel` carries an empty `ToolSet`), so the run
        // cannot re-arm the loop. See docs/agent/world/ecs-runtime.md (Loop guard).
        if final_wrap_up {
            entity.history.0.push(Msg {
                role: Role::User,
                content: vec![Block::Text {
                    text: LOOP_CAP_WRAP_UP.into(),
                }],
            });
        }
        // Advance the loop-guard counter — World state, so replay reproduces the brake.
        entity.turns = next_turns;
        entity.activity = Activity::Thinking { cmd };
        // Entity model override else world default (Resources.model).
        let params = entity.model.clone().unwrap_or(default_model);
        let messages = entity.history.clone();

        let command = Command::CallModel {
            cmd,
            entity: *to,
            messages,
            // Withhold tools on the FINAL wrap-up turn so the run concludes instead of
            // re-arming the loop; otherwise offer the surface entity's tools (GAP A).
            tools: if final_wrap_up {
                ToolSet::default()
            } else {
                surface_tools
            },
            params,
            key: CommandKey,
        };
        (world, vec![command])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::History;
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, Tick,
    };

    /// An `Idle` primary entity whose per-entity `loop_cap` is `cap` (`0` ⇒ unbounded).
    fn world_with_loop_cap(cap: u32) -> World {
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        };
        let mut world = World::new(0, Resources::new(42, model));
        let budget = crate::agent::world::budget::Budget {
            limits: crate::agent::world::budget::Limits {
                loop_cap: cap,
                ..Default::default()
            },
            ..Default::default()
        };
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History::default(),
                activity: Activity::Idle,
                gate: EntityGate::default(),
                budget,
                inbox: Inbox::default(),
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

    /// A `ModelResponded·EndTurn` that closes the turn `cmd` so the entity returns to
    /// `Idle` and the next `UserMessage` can initiate the following turn.
    fn end_turn(cmd: CmdId, at: Tick) -> Event {
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
                    usage: Usage::default(),
                    model_id: "claude-x".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    fn call_model_cmd(commands: &[Command]) -> CmdId {
        match commands.first() {
            Some(Command::CallModel { cmd, .. }) => *cmd,
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    /// Whether a `CallModel`'s request History carries the loop-cap wrap-up directive.
    fn carries_wrap_up(commands: &[Command]) -> bool {
        match commands.first() {
            Some(Command::CallModel { messages, .. }) => messages.0.iter().any(|m| {
                matches!(m.role, Role::User)
                    && m.content.iter().any(|b| matches!(b, Block::Text { text } if text == LOOP_CAP_WRAP_UP))
            }),
            _ => false,
        }
    }

    fn turns_of(world: &World) -> u32 {
        world.entities.get(&0).expect("entity").turns
    }

    /// VC-2.3 (loop guard): reaching `loop_cap` forces ONE final wrap-up turn (the
    /// turn that reaches the cap injects the wrap-up directive) and then STOPS — a
    /// further `UserMessage` initiates no turn. The counter is World state, so the
    /// brake replays deterministically (Inv 11).
    #[test]
    fn loop_cap_forces_a_final_wrap_up_turn_then_stops() {
        let world = world_with_loop_cap(2);

        // Turn 1 (turns 0 → 1): a NORMAL turn — no wrap-up directive.
        let (world, c1) = tick(&world, &user_message("u1", 1));
        assert_eq!(turns_of(&world), 1, "the first initiation advances the counter to 1");
        assert!(!carries_wrap_up(&c1), "a sub-cap turn carries no wrap-up directive");
        let cmd1 = call_model_cmd(&c1);
        let (world, _) = tick(&world, &end_turn(cmd1, 2));
        assert!(matches!(world.entities.get(&0).expect("e").activity, Activity::Idle));

        // Turn 2 (turns 1 → 2 == cap): the FINAL wrap-up turn — directive injected.
        let (world, c2) = tick(&world, &user_message("u2", 3));
        assert_eq!(turns_of(&world), 2, "the cap-reaching initiation advances to the cap");
        assert!(
            carries_wrap_up(&c2),
            "the cap-reaching turn injects the wrap-up directive (forces the model to finalize)"
        );
        let cmd2 = call_model_cmd(&c2);
        let (world, _) = tick(&world, &end_turn(cmd2, 4));
        assert!(matches!(world.entities.get(&0).expect("e").activity, Activity::Idle));

        // Turn 3 attempt (turns 2 >= cap): STOP — no further turn is initiated.
        let history_before = world.entities.get(&0).expect("e").history.0.len();
        let (world, c3) = tick(&world, &user_message("u3", 5));
        assert!(c3.is_empty(), "past the loop cap no further turn is initiated (stop)");
        let e = world.entities.get(&0).expect("e");
        assert!(matches!(e.activity, Activity::Idle), "the looped-out entity stays Idle");
        assert_eq!(turns_of(&world), 2, "the counter does not advance past the cap");
        assert_eq!(
            e.history.0.len(),
            history_before,
            "a stopped initiation consumes no message into History"
        );
    }

    /// A `loop_cap` of 0 is UNBOUNDED: no turn ever injects the wrap-up directive and
    /// no initiation is ever stopped, however many turns run (the brake is off).
    #[test]
    fn loop_cap_zero_is_unbounded_no_wrap_up_no_stop() {
        let mut world = world_with_loop_cap(0);
        for turn in 0..4u32 {
            let (next, cmds) = tick(&world, &user_message("u", (turn * 2 + 1) as Tick));
            assert_eq!(cmds.len(), 1, "an unbounded entity always initiates the turn");
            assert!(!carries_wrap_up(&cmds), "an unbounded entity never injects a wrap-up directive");
            let cmd = call_model_cmd(&cmds);
            let (next, _) = tick(&next, &end_turn(cmd, (turn * 2 + 2) as Tick));
            world = next;
        }
        assert_eq!(turns_of(&world), 4, "the counter still tracks turns even when unbounded");
    }

    /// (A) GAP A: a surface-capable ROOT entity (the App populated
    /// `Resources.surface_tools`) carries those tools in its Intake `CallModel`'s
    /// `ToolSet`, so the live driver's `resolve_tools`/`MessageBuilder` declare the
    /// tool on the wire and the model can return a `set_value` tool_use. A pre-tools
    /// entity (empty `surface_tools`) still emits an EMPTY `ToolSet`, byte-identical
    /// to a tool-less call (replay/fingerprint stability).
    #[test]
    fn surface_capable_entity_offers_set_value_on_its_call_model() {
        // The App offers the root entity `set_value` — seed via the production
        // SessionStarted fold (SurfaceSystem::step) so it is the sole assignment site.
        let world = world_with_loop_cap(0);
        let session_event = Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input: LogicalInput::SessionStarted {
                seed: 42,
                surface_tools: vec!["set_value".into()],
            },
        };
        let (world, _) = tick(&world, &session_event);

        let (_world, commands) = tick(&world, &user_message("type into the field", 1));
        match commands.first() {
            Some(Command::CallModel { tools, .. }) => assert_eq!(
                tools,
                &ToolSet(vec!["set_value".into()]),
                "a surface-capable entity's CallModel offers set_value (GAP A)"
            ),
            other => panic!("expected CallModel, got {other:?}"),
        }

        // A pre-tools entity (no surface tools) still emits an EMPTY ToolSet.
        let plain = world_with_loop_cap(0);
        let (_w, plain_cmds) = tick(&plain, &user_message("hi", 1));
        match plain_cmds.first() {
            Some(Command::CallModel { tools, .. }) => assert_eq!(
                tools,
                &ToolSet::default(),
                "no surface tools ⇒ an empty ToolSet (byte-identical to a pre-tools call)"
            ),
            other => panic!("expected CallModel, got {other:?}"),
        }
    }
}
