//! guardrail — the GuardrailSystem (phase 1 Lifecycle). See
//! docs/agent/world/ecs-runtime.md (WorldGate; PolicyHalt(GuardrailTrip);
//! Invariant 13; gate transition tables §1857-1882).
//!
//! GuardrailSystem is the TRIP SOURCE for an App-wide policy halt: on a concrete
//! policy-predicate match it ADDS a `PolicyHalt(GuardrailTrip)` hold to the
//! App-wide `WorldGate`, closing the gate so NEW WORK is gated (turn initiation in
//! IntakeSystem, turn-advance/continuation in ToolSystem) — Invariant 13. The hold
//! is APPENDED, never substituted, so a policy halt coexists with a concurrent
//! `User` pause (and other policy trips); the gate returns to `Open` only when ALL
//! holds clear (`is_open() ⇔ holds empty`). It runs in phase 1 (Lifecycle) so the
//! trip lands BEFORE the new-work phases (Intake phase 2, TurnAdvance phase 5) that
//! the gate suppresses.
//!
//! This System owns ONLY the trip; the NAMED clearing input that drains the hold is
//! `ClearPolicyHalt { trip, authority }`, folded by GateSystem (Invariant 15) — its
//! authority enforcement is a separate concern, out of scope here.
//!
//! Distinct from BudgetSystem's `budget_exhausted_trip()`, which sets a per-entity
//! `EntityHalt` (one entity, not the App); this is the App-WIDE WorldGate trip.

use super::{Input, System};
use crate::agent::world::effects::Command;
use crate::agent::world::gates::{GuardrailTrip, PauseReason};
use crate::agent::world::inputs::{LogicalInput, StopReason};
use crate::agent::world::world::World;

/// The stable `GuardrailTrip` a model-refusal policy halt stands on. Its NAMED
/// clearing input is `ClearPolicyHalt { trip, authority }` carrying THIS exact
/// trip (Invariant 15), folded by GateSystem — so the policy halt is never a
/// permanent sink. Mirrors `budget::budget_exhausted_trip()` as the single source
/// of truth for the trip identifier, reused by the clearing path.
pub fn model_refusal_trip() -> GuardrailTrip {
    GuardrailTrip("model-refusal".into())
}

/// The P0 policy predicate — ONE concrete, testable rule: a model inference whose
/// `stop_reason` is `Refusal` is a content-policy violation that must halt the
/// World. Returns the `GuardrailTrip` to trip on a match, else `None`. PURE: a
/// function of the input alone — no World read, no clock/RNG/IO (Invariant 1).
///
/// The predicate set is deliberately narrow for P0; richer guardrail rules
/// (configured denied-term lists, tool-use policy flags) extend this function
/// later without touching the System's trip mechanism. See
/// docs/agent/world/ecs-runtime.md (PolicyHalt(GuardrailTrip)).
fn policy_violation(input: &Input) -> Option<GuardrailTrip> {
    match input {
        LogicalInput::ModelResponded { meta, .. } if meta.stop_reason == StopReason::Refusal => {
            Some(model_refusal_trip())
        }
        _ => None,
    }
}

/// GuardrailSystem — trips an App-wide `WorldGate` `PolicyHalt(GuardrailTrip)` hold
/// on a policy-predicate match (phase 1 of the tick). Pure reducer; emits no
/// Commands — it only ADDS a hold to `WorldGate.holds`.
pub struct GuardrailSystem;

impl System for GuardrailSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match policy_violation(input) {
            // A policy violation ADDS one `PolicyHalt` hold (idempotent per trip),
            // closing the gate. The push never replaces existing holds, so the
            // policy halt coexists with a concurrent `User` pause and the gate
            // reopens only when ALL holds clear (Invariant 13).
            Some(trip) => {
                let hold = PauseReason::PolicyHalt(trip);
                let mut world = world.clone();
                if !world.resources.gate.holds.contains(&hold) {
                    world.resources.gate.holds.push(hold);
                }
                (world, Vec::new())
            }
            // A non-matching input is byte-identical to today: no hold, no Command.
            None => (world.clone(), Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::gates::{EntityGate, GuardrailTrip, PauseReason};
    use crate::agent::world::history::{Block, History};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Lineage, ModelConfig, Resources, Tick, World,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// A World with one primary entity carrying the given `activity`.
    fn world_with(activity: Activity) -> World {
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

    /// A bare `ModelResponded` input carrying the given `stop_reason`.
    fn responded_input(stop: StopReason) -> LogicalInput {
        LogicalInput::ModelResponded {
            cmd: 0,
            entity: 0,
            fingerprint: Fingerprint("fp".into()),
            blocks: vec![Block::Text {
                text: "no".into(),
            }],
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-x".into(),
                stop_reason: stop,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        }
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

    fn responded_event(cmd: CmdId, stop: StopReason, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::Text {
                    text: "I can't help with that.".into(),
                }],
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

    // -----------------------------------------------------------------------
    // (a) a matching input trips EXACTLY ONE PolicyHalt hold (gate Paused)
    // -----------------------------------------------------------------------

    /// VC-4.1 (trip): a `Refusal` `ModelResponded` adds exactly one
    /// `PolicyHalt(GuardrailTrip)` hold — the WorldGate closes (Paused). No Command.
    #[test]
    fn refusal_trips_exactly_one_policy_halt() {
        let world = world_with(Activity::Idle);
        assert!(world.resources.gate.is_open(), "a fresh gate is open");

        let (world, commands) = GuardrailSystem.step(&world, &responded_input(StopReason::Refusal));

        assert!(commands.is_empty(), "GuardrailSystem emits no Commands");
        assert_eq!(
            world.resources.gate.holds,
            vec![PauseReason::PolicyHalt(model_refusal_trip())],
            "a refusal trips exactly one PolicyHalt hold"
        );
        assert!(!world.resources.gate.is_open(), "the World is Paused (gate closed)");
    }

    // -----------------------------------------------------------------------
    // (b) a non-matching input adds NONE (gate stays Open, byte-identical)
    // -----------------------------------------------------------------------

    /// VC-4.1 (no false trip): a non-`Refusal` result adds no hold; the gate stays
    /// Open and the World is BYTE-IDENTICAL (proved via canonical JSON equality).
    #[test]
    fn non_refusal_adds_no_hold_and_is_byte_identical() {
        let world = world_with(Activity::Idle);
        let before = serde_json::to_string(&world).expect("serialise");

        let (next, commands) = GuardrailSystem.step(&world, &responded_input(StopReason::EndTurn));

        assert!(commands.is_empty());
        assert!(next.resources.gate.holds.is_empty(), "no hold added on a non-match");
        assert!(next.resources.gate.is_open(), "the gate stays Open");
        let after = serde_json::to_string(&next).expect("serialise");
        assert_eq!(before, after, "a non-matching input leaves the World byte-identical");

        // Other lifecycle/exogenous inputs are likewise orthogonal to the guardrail.
        let (next, _) =
            GuardrailSystem.step(&world, &user_message("hello", 1).input);
        assert!(next.resources.gate.is_open(), "a UserMessage never trips the guardrail");
    }

    // -----------------------------------------------------------------------
    // (c) the PolicyHalt hold COEXISTS with a pre-existing User hold
    // -----------------------------------------------------------------------

    /// VC-4.1 (multi-hold): a guardrail-tripped `PolicyHalt` is APPENDED beside a
    /// pre-existing `User` pause — both holds live in the set; the gate stays closed.
    #[test]
    fn policy_halt_coexists_with_user_hold() {
        let mut world = world_with(Activity::Idle);
        // A user pause already holds the gate (an independent source).
        world.resources.gate.holds.push(PauseReason::User);
        assert!(!world.resources.gate.is_open());

        let (world, _) = GuardrailSystem.step(&world, &responded_input(StopReason::Refusal));

        assert_eq!(
            world.resources.gate.holds,
            vec![
                PauseReason::User,
                PauseReason::PolicyHalt(model_refusal_trip()),
            ],
            "the PolicyHalt coexists with the User hold (multi-hold, appended not replaced)"
        );
        assert!(!world.resources.gate.is_open(), "the gate is closed by both holds");
    }

    /// The trip is idempotent per source: a second refusal does not duplicate the
    /// `PolicyHalt` hold (mirrors GateSystem's `Pause` idempotency).
    #[test]
    fn refusal_trip_is_idempotent_per_source() {
        let world = world_with(Activity::Idle);
        let (world, _) = GuardrailSystem.step(&world, &responded_input(StopReason::Refusal));
        let (world, _) = GuardrailSystem.step(&world, &responded_input(StopReason::Refusal));
        assert_eq!(
            world.resources.gate.holds,
            vec![PauseReason::PolicyHalt(model_refusal_trip())],
            "a repeated refusal does not duplicate the hold"
        );
    }

    /// The pure predicate matches a `Refusal` and only a `Refusal` — the directly
    /// testable policy rule both branches drive.
    #[test]
    fn policy_violation_predicate_matches_only_refusal() {
        assert_eq!(
            policy_violation(&responded_input(StopReason::Refusal)),
            Some(model_refusal_trip()),
            "a refusal is a policy violation"
        );
        for stop in [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::MaxTokens,
            StopReason::PauseTurn,
        ] {
            assert_eq!(
                policy_violation(&responded_input(stop)),
                None,
                "{stop:?} is not a policy violation"
            );
        }
        assert_eq!(
            policy_violation(&user_message("hi", 1).input),
            None,
            "a UserMessage is not a policy violation"
        );
    }

    // -----------------------------------------------------------------------
    // Registration + phase: the trip is wired into the tick and gates NEW WORK
    // -----------------------------------------------------------------------

    /// Proves GuardrailSystem is REGISTERED at the right phase: a `Refusal` landing
    /// through the full `tick` (a) settles into History gate-PROOF (Inv 13) AND
    /// (b) closes the WorldGate, so a subsequent `UserMessage` is GATED — no new
    /// turn initiates while the policy halt stands.
    #[test]
    fn tick_registers_guardrail_and_a_tripped_halt_gates_new_work() {
        // Start a turn so the entity is Thinking on a known cmd.
        let world = world_with(Activity::Idle);
        let (world, commands) = tick(&world, &user_message("do the thing", 1));
        let cmd = call_model_cmd(&commands);

        // A refusal lands: TurnSystem settles it (gate-proof) and GuardrailSystem
        // trips the WorldGate in the same tick.
        let (world, _) = tick(&world, &responded_event(cmd, StopReason::Refusal, 2));
        let e = world.entities.get(&0).expect("entity");
        assert!(matches!(e.activity, Activity::Idle), "the refusal settled the turn (gate-proof)");
        assert_eq!(e.history.0.len(), 2, "user + refusal recorded");
        assert_eq!(
            world.resources.gate.holds,
            vec![PauseReason::PolicyHalt(model_refusal_trip())],
            "the guardrail tripped a PolicyHalt hold via the registry"
        );
        assert!(!world.resources.gate.is_open(), "the World is Paused by the policy halt");

        // NEW WORK is gated: a fresh UserMessage does not initiate while halted.
        let (world, commands) = tick(&world, &user_message("again", 3));
        assert!(commands.is_empty(), "no CallModel while the policy halt stands (Inv 13)");
        assert!(
            matches!(world.entities.get(&0).expect("entity").activity, Activity::Idle),
            "the entity stays Idle — initiation is deferred behind the policy halt"
        );

        // The unused `GuardrailTrip` import is exercised by the trip identifier.
        let _: GuardrailTrip = model_refusal_trip();
    }
}
