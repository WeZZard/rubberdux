//! gate — the GateSystem (phase 1 Lifecycle). See docs/agent/world/ecs-runtime.md.
//!
//! GateSystem owns gate STATE — it folds the lifecycle inputs that open and close
//! the App-wide `WorldGate` and the per-entity `EntityGate`, and emits NO effect
//! Commands. A closed gate blocks NEW WORK (turn initiation in IntakeSystem,
//! turn-advance/continuation in ToolSystem), but result-settling is gate-PROOF and
//! never consults the gate (Invariant 13). The folds:
//!
//! - `Pause { reason }` — push `reason` onto the WorldGate's `holds` (idempotent per
//!   source), closing the gate. A `Pause` carries no entity, so it is App-wide.
//! - `Resume` — drain every `User` hold from the WorldGate; a `PolicyHalt` survives
//!   a user resume (per-source clearing).
//! - `ClearPolicyHalt { trip, authority }` — the NAMED clearing input (Invariant
//!   15): remove the matching `PolicyHalt(trip)` WorldGate hold AND clear any
//!   per-entity `EntityHalt { trip }`, reopening the gate once nothing else holds it.
//!   The `authority` token's VERIFICATION is a later enforcement concern; the
//!   clearing transition exists here.

use super::{Input, System};
use crate::agent::world::effects::Command;
use crate::agent::world::gates::{EntityHalt, PauseReason};
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::World;

/// GateSystem — folds the pause/resume/clear inputs into gate state (phase 1 of
/// the tick). Emits no Commands; it only updates `WorldGate`/`EntityGate`.
pub struct GateSystem;

impl System for GateSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // Add an App-wide pause hold (idempotent per source) — closes the gate.
            LogicalInput::Pause { reason } => {
                let mut world = world.clone();
                if !world.resources.gate.holds.contains(reason) {
                    world.resources.gate.holds.push(reason.clone());
                }
                (world, Vec::new())
            }
            // Drain every `User` hold; a `PolicyHalt` survives a user resume.
            LogicalInput::Resume => {
                let mut world = world.clone();
                world
                    .resources
                    .gate
                    .holds
                    .retain(|reason| !matches!(reason, PauseReason::User));
                (world, Vec::new())
            }
            // The named clearing input (Inv 15): clear the matching policy halt at
            // BOTH levels — the WorldGate `PolicyHalt(trip)` hold and any entity's
            // `EntityHalt { trip }`.
            LogicalInput::ClearPolicyHalt { trip, .. } => {
                let mut world = world.clone();
                world.resources.gate.holds.retain(
                    |reason| !matches!(reason, PauseReason::PolicyHalt(t) if t == trip),
                );
                for entity in world.entities.values_mut() {
                    if matches!(&entity.gate.halt, Some(EntityHalt { trip: t }) if t == trip) {
                        entity.gate.halt = None;
                    }
                }
                (world, Vec::new())
            }
            // Every other input is orthogonal to the gates (handled by other phases).
            _ => (world.clone(), Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::world::effects::Command;
    use crate::agent::world::gates::{Authority, EntityGate, EntityHalt, GuardrailTrip, PauseReason};
    use crate::agent::world::history::{History, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, Tick, World,
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

    fn pause(reason: PauseReason, at: Tick) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::Pause { reason },
        }
    }

    fn resume(at: Tick) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::Resume,
        }
    }

    fn clear_policy_halt(trip: &str, at: Tick) -> Event {
        Event {
            origin: Origin::Human,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ClearPolicyHalt {
                trip: GuardrailTrip(trip.into()),
                authority: Authority("admin".into()),
            },
        }
    }

    fn model_responded(cmd: CmdId, text: &str, stop: StopReason, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![crate::agent::world::history::Block::Text { text: text.into() }],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: "claude-x".into(),
                    stop_reason: stop,
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

    /// Pause adds a hold to the WorldGate so the gate reports CLOSED (Inv 13).
    #[test]
    fn pause_closes_the_world_gate() {
        let world = idle_world();
        assert!(world.resources.gate.is_open(), "fresh gate is open");

        let (world, commands) = tick(&world, &pause(PauseReason::User, 1));
        assert!(commands.is_empty(), "GateSystem emits no Commands");
        assert!(!world.resources.gate.is_open(), "Pause closes the gate");
        assert_eq!(world.resources.gate.holds, vec![PauseReason::User]);

        // Idempotent per source: a second identical Pause does not duplicate the hold.
        let (world, _) = tick(&world, &pause(PauseReason::User, 2));
        assert_eq!(
            world.resources.gate.holds,
            vec![PauseReason::User],
            "Pause is idempotent per source"
        );
    }

    /// While paused, a `UserMessage` does NOT initiate a turn — IntakeSystem defers
    /// it (the entity stays `Idle`, nothing is dispatched). (Inv 13.)
    #[test]
    fn paused_user_message_does_not_initiate() {
        let world = idle_world();
        let (world, _) = tick(&world, &pause(PauseReason::User, 1));

        let (world, commands) = tick(&world, &user_message("hello", 2));
        assert!(commands.is_empty(), "no CallModel while the gate is closed");
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Idle),
            "a paused entity stays Idle — initiation is deferred"
        );
        assert!(e.history.0.is_empty(), "the deferred message is not consumed");
    }

    /// Inv 13 (the core VC-1.2 edge): an in-flight `ModelResponded(EndTurn)` STILL
    /// settles into History and reaches `Idle` even while paused — result-settling
    /// is gate-PROOF; only the NEXT continuation would be deferred.
    #[test]
    fn in_flight_model_responded_settles_while_paused() {
        let world = idle_world();
        // Start a turn (entity → Thinking) BEFORE pausing.
        let (world, commands) = tick(&world, &user_message("hi", 1));
        let cmd = call_model_cmd(&commands);
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Thinking { .. }
        ));

        // Pause while the inference is in flight.
        let (world, _) = tick(&world, &pause(PauseReason::User, 2));
        assert!(!world.resources.gate.is_open(), "gate closed mid-turn");

        // The in-flight result lands DURING the pause: it must settle regardless.
        let (world, commands) =
            tick(&world, &model_responded(cmd, "the answer", StopReason::EndTurn, 3));
        assert!(commands.is_empty(), "EndTurn emits no continuation");
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Idle),
            "ModelResponded settles Thinking → Idle even while paused (Inv 13)"
        );
        assert_eq!(e.history.0.len(), 2, "user + assistant: the result was recorded");
        assert!(matches!(
            &e.history.0[1],
            Msg { role: Role::Assistant, .. }
        ));
        // The gate is still closed — settling did not clear the pause.
        assert!(!world.resources.gate.is_open(), "the pause hold survives settling");
    }

    /// `Resume` drains the `User` hold, reopening the gate so deferred work
    /// proceeds: a `UserMessage` that was a no-op while paused now initiates. (Inv 13.)
    #[test]
    fn resume_drains_hold_and_deferred_work_proceeds() {
        let world = idle_world();
        let (world, _) = tick(&world, &pause(PauseReason::User, 1));

        // Deferred while paused.
        let (world, commands) = tick(&world, &user_message("first", 2));
        assert!(commands.is_empty());
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Idle
        ));

        // Resume drains the hold → gate open.
        let (world, commands) = tick(&world, &resume(3));
        assert!(commands.is_empty(), "Resume emits no Commands");
        assert!(world.resources.gate.is_open(), "Resume reopens the gate");

        // The same message now initiates a turn.
        let (world, commands) = tick(&world, &user_message("second", 4));
        assert_eq!(commands.len(), 1, "deferred work proceeds after Resume");
        let e = world.entities.get(&0).expect("entity");
        assert!(matches!(e.activity, Activity::Thinking { .. }));
    }

    /// `Resume` clears `User` holds but a `PolicyHalt` survives — the gate stays
    /// closed until its own named clearing input arrives (per-source clearing).
    #[test]
    fn resume_does_not_clear_a_policy_halt() {
        let world = idle_world();
        let (world, _) = tick(&world, &pause(PauseReason::User, 1));
        let (world, _) = tick(
            &world,
            &pause(PauseReason::PolicyHalt(GuardrailTrip("policy-9".into())), 2),
        );
        assert_eq!(world.resources.gate.holds.len(), 2);

        // Resume removes only the User hold; the PolicyHalt remains.
        let (world, _) = tick(&world, &resume(3));
        assert_eq!(
            world.resources.gate.holds,
            vec![PauseReason::PolicyHalt(GuardrailTrip("policy-9".into()))],
            "a PolicyHalt survives a user Resume"
        );
        assert!(!world.resources.gate.is_open(), "gate still closed by the policy halt");

        // ClearPolicyHalt with the matching trip reopens it.
        let (world, _) = tick(&world, &clear_policy_halt("policy-9", 4));
        assert!(world.resources.gate.is_open(), "ClearPolicyHalt reopens the gate");
    }

    /// Inv 15: `ClearPolicyHalt` is the NAMED clearing input for a per-entity policy
    /// `EntityHalt`. A halted entity defers initiation; clearing the halt by its
    /// matching `trip` reopens the EntityGate and deferred work proceeds.
    #[test]
    fn clear_policy_halt_clears_entity_halt_and_reopens_gate() {
        let mut world = idle_world();
        // The entity stands on a policy halt (set by an enforcement System upstream;
        // BudgetSystem is out of scope here — we construct the standing halt directly).
        world.entities.get_mut(&0).expect("entity").gate.halt = Some(EntityHalt {
            trip: GuardrailTrip("content-filter".into()),
        });
        assert!(
            !world.entities.get(&0).expect("entity").gate.is_open(),
            "a halted entity gate is closed"
        );

        // While halted, a UserMessage does NOT initiate (the EntityGate is closed).
        let (world, commands) = tick(&world, &user_message("blocked", 1));
        assert!(commands.is_empty(), "a halted entity defers initiation");
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Idle
        ));

        // ClearPolicyHalt carrying the matching trip clears the EntityHalt (Inv 15).
        let (world, commands) = tick(&world, &clear_policy_halt("content-filter", 2));
        assert!(commands.is_empty(), "GateSystem emits no Commands");
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(e.gate.halt, None, "the matching EntityHalt was cleared");
        assert!(e.gate.is_open(), "the EntityGate reopened");

        // Deferred work now proceeds.
        let (world, commands) = tick(&world, &user_message("now allowed", 3));
        assert_eq!(commands.len(), 1, "the entity can initiate once the halt clears");
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Thinking { .. }
        ));
    }

    /// A non-matching `ClearPolicyHalt` (different `trip`) leaves the halt standing.
    #[test]
    fn clear_policy_halt_only_clears_the_matching_trip() {
        let mut world = idle_world();
        world.entities.get_mut(&0).expect("entity").gate.halt = Some(EntityHalt {
            trip: GuardrailTrip("trip-a".into()),
        });

        let (world, _) = tick(&world, &clear_policy_halt("trip-b", 1));
        assert_eq!(
            world.entities.get(&0).expect("entity").gate.halt,
            Some(EntityHalt {
                trip: GuardrailTrip("trip-a".into())
            }),
            "a non-matching trip does not clear the halt"
        );
    }
}
