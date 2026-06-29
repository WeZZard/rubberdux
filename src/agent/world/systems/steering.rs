//! steering — the SteeringSystem (phase 2 Intake). See docs/agent/world/ecs-runtime.md
//! (SteeringSystem; "Steering is NOT an Input — it is UserMessage + optional Cancel";
//! Verification VC-1.7).
//!
//! Steering is a COMPOSITION, never its own primitive: a steered turn is just a
//! `UserMessage` plus an OPTIONAL `Cancel` (handled by CancelSystem). There is no
//! `Steer` input. SteeringSystem owns only the `UserMessage` half: a message that
//! arrives while the addressed entity is MID-RUN (`Thinking`/`ResolvingToolUses`/
//! `Compacting`/`Cancelling`) MUST NOT interrupt the in-flight turn. It is held for
//! the next turn initiation rather than folded into the current one — IntakeSystem
//! admits a queued message only once the entity is back to `Idle` and the gates are
//! Open. A message to an `Idle` entity is left entirely to IntakeSystem (phase 2),
//! which starts the turn directly; SteeringSystem never starts a turn.
//!
//! Durable queueing: a mid-run `UserMessage` is PARKED on the per-entity
//! `Components.inbox` FIFO so it is honoured on the next turn — IntakeSystem DRAINS
//! the inbox into the turn it initiates from `Idle`. SteeringSystem owns the
//! enqueue (the non-interrupt half); the inbox is UNBOUNDED for P1a (the capped,
//! drop-oldest backpressure is P2).
//!
//! Phase ordering (Inv 12): SteeringSystem is registered BEFORE IntakeSystem within
//! phase 2. This means SteeringSystem sees the PRE-admission `Activity`: if the
//! entity is `Idle`, it no-ops (IntakeSystem will admit); if the entity is in any
//! active state, it enqueues. This structural ordering removes any need for content-
//! equality heuristics — EVERY genuine mid-run `UserMessage`, including repeated
//! identical text, is durably parked exactly once.

use super::{Input, System};
use crate::agent::world::effects::Command;
use crate::agent::world::history::Block;
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::lifecycle::{DropReason, LifecycleEvent};
use crate::agent::world::world::{Activity, EdgeId, Inbox, WallClock, World};

/// SteeringSystem — the `UserMessage` half of steering (phase 2 of the tick): a
/// mid-run message does not interrupt the in-flight turn.
pub struct SteeringSystem;

impl System for SteeringSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        // Steering acts only on a `UserMessage`; every other input is orthogonal to it.
        let LogicalInput::UserMessage { to, text } = input else {
            return (world.clone(), Vec::new());
        };

        // SteeringSystem runs BEFORE IntakeSystem in phase 2 (Inv 12), so it sees the
        // PRE-admission `Activity`. If the entity is `Idle` (or absent), IntakeSystem
        // will admit the message this same tick — no-op here. If the entity is in ANY
        // active state, the message is genuinely mid-run and must be enqueued FIFO.
        let mid_run = world
            .entities
            .get(to)
            .map(|e| {
                matches!(
                    e.activity,
                    Activity::Thinking { .. }
                        | Activity::ResolvingToolUses { .. }
                        | Activity::Compacting { .. }
                        | Activity::Cancelling { .. }
                )
            })
            .unwrap_or(false);

        if !mid_run {
            // `Idle` or absent: IntakeSystem owns admission — nothing to steer.
            return (world.clone(), Vec::new());
        }

        // NON-INTERRUPT + DURABLE ENQUEUE: the in-flight turn's `Activity` and History
        // are left untouched (the steered message never folds into the current turn),
        // and the message is PARKED on the entity's Inbox (FIFO) so IntakeSystem
        // honours it on the next turn initiation from `Idle`. Every genuinely mid-run
        // `UserMessage` is enqueued, including repeated identical text — the phase
        // ordering is the sole guard; there is no content-equality heuristic.
        let mut next = world.clone();
        // The Inbox is BOUNDED (Boundedness, Inv 11): read the entity's capacity and the
        // last observed wall-time, then enqueue with DropOldest backpressure. The
        // eviction is World state (replay-safe). The `MessageDropped` notice it returns
        // is stratum-2 observability surfaced to the log by the driver (as the sole
        // appender of lifecycle records, like `CommandDispatched`) — so the human sees
        // the loss yet it never folds into the World on replay (the two-strata rule).
        let capacity = next
            .entities
            .get(to)
            .map(|e| e.budget.limits.inbox_capacity)
            .unwrap_or(0);
        let wall = next.resources.wall;
        let mut commands = Vec::new();
        if let Some(entity) = next.entities.get_mut(to) {
            // The eviction is already folded into the World above (stratum-1, so
            // replay reconstructs the bounded Inbox deterministically). The drop
            // NOTICE is stratum-2 observability: carry it to the driver via
            // `Command::EmitLifecycle` so the LIVE driver appends it through
            // `append_lifecycle` (the sole lifecycle appender) and the REPLAY driver
            // discards it — keeping replay byte-identical. The notice is never
            // discarded here.
            if let Some(notice) = bounded_enqueue(
                &mut entity.inbox,
                vec![Block::Text { text: text.clone() }],
                capacity,
                wall,
            ) {
                commands.push(Command::EmitLifecycle(notice));
            }
        }
        (next, commands)
    }
}

/// The human edge a DropOldest notice is stamped to. The Inbox holds inbound human
/// `UserMessage`s, so an eviction is reported on the human edge. A System sees only
/// the `LogicalInput`, not the Event envelope's `edge`, and the human edge is the sole
/// counterpart in this phase; threading the real edge via the driver is a later
/// milestone. See docs/agent/world/ecs-runtime.md (Bounded queues — Inbox DropOldest).
const HUMAN_EDGE: EdgeId = 0;

/// Bounded FIFO enqueue with DropOldest backpressure (Boundedness, Inv 11): push
/// `content` onto `inbox`, first evicting the OLDEST (front) entries so the post-push
/// length never exceeds `capacity` (`capacity == 0` ⇒ unbounded — never evicts). The
/// eviction is World state, so the brake replays deterministically. Returns the
/// stratum-2 `LifecycleEvent::MessageDropped` recording HOW MANY entries were evicted,
/// or `None` if none were — the observable drop notice (Theme 5d), stamped with the
/// observed `wall` and the human edge. The queue stays FIFO. See
/// docs/agent/world/ecs-runtime.md (Bounded queues — Inbox DropOldest).
fn bounded_enqueue(
    inbox: &mut Inbox,
    content: Vec<Block>,
    capacity: u32,
    wall: WallClock,
) -> Option<LifecycleEvent> {
    let mut dropped: u32 = 0;
    if capacity != 0 {
        // Evict from the FRONT (oldest first) until the new entry fits within the cap.
        // A `while` (not a single `remove`) also re-bounds a queue whose cap was lowered.
        while inbox.pending.len() as u64 >= capacity as u64 {
            inbox.pending.remove(0);
            dropped = dropped.saturating_add(1);
        }
    }
    inbox.pending.push(content);
    (dropped > 0).then_some(LifecycleEvent::MessageDropped {
        at: wall,
        edge: HUMAN_EDGE,
        reason: DropReason::InboxOverflow,
        dropped,
    })
}

#[cfg(test)]
mod tests {
    use crate::agent::world::budget::Budget;
    use crate::agent::world::effects::Command;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, Tick,
        World,
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
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );
        world
    }

    /// `idle_world` advanced into a mid-run turn: the entity is `Thinking { cmd }`
    /// with `starter` as the turn-opening user message in History. Used to exercise
    /// the steer/park path without IntakeSystem admitting this tick.
    fn thinking_world(starter: &str, cmd: CmdId) -> World {
        let mut world = idle_world();
        let e = world.entities.get_mut(&0).expect("entity");
        e.history.0.push(Msg {
            role: Role::User,
            content: vec![Block::Text {
                text: starter.into(),
            }],
        });
        e.activity = Activity::Thinking { cmd };
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

    fn model_responded(cmd: CmdId, text: &str, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::Text { text: text.into() }],
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

    /// VC-1.7: a mid-run `UserMessage` (entity `Thinking`) does NOT interrupt the
    /// in-flight turn — the entity stays `Thinking` on the SAME `cmd`, no new
    /// `CallModel` is emitted, and the message is not folded into the current turn's
    /// History. Once the turn ENDS and the entity is `Idle`, a `UserMessage`
    /// initiates the next turn (honoured on the next turn), proving steering is a
    /// composition that redirects across turns without interrupting the current one.
    #[test]
    fn mid_run_user_message_does_not_interrupt_and_is_honoured_next_turn() {
        let world = idle_world();

        // Turn 1 starts (Idle → Thinking on cmd0); History holds just the first msg.
        let (world, commands) = tick(&world, &user_message("first", 1));
        let cmd0 = call_model_cmd(&commands);
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Thinking { cmd } if cmd == cmd0
        ));
        let history_after_start = world.entities.get(&0).expect("entity").history.0.len();
        assert_eq!(history_after_start, 1, "only the first user message so far");

        // A second message arrives MID-RUN: it must not interrupt.
        let (world, commands) = tick(&world, &user_message("second", 2));
        assert!(
            commands.is_empty(),
            "a mid-run UserMessage emits no Command (no new turn started)"
        );
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { cmd } if cmd == cmd0),
            "the in-flight turn is untouched — still Thinking on the same cmd"
        );
        assert_eq!(
            e.history.0.len(),
            history_after_start,
            "the steered message is NOT folded into the current turn's History"
        );

        // Turn 1 ends → Idle, with the assistant answer appended.
        let (world, commands) = tick(&world, &model_responded(cmd0, "answer", 3));
        assert!(commands.is_empty(), "EndTurn emits no continuation");
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Idle
        ));

        // The NEXT turn initiation honours a user message (the run was never blocked
        // by the mid-run steer): Idle → Thinking again, exactly one CallModel.
        let (world, commands) = tick(&world, &user_message("second", 4));
        assert_eq!(commands.len(), 1, "the next turn initiates normally");
        let cmd1 = call_model_cmd(&commands);
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { cmd } if cmd == cmd1),
            "honoured on the next turn — a fresh turn starts once Idle"
        );
        assert_ne!(cmd0, cmd1, "the next turn carries a freshly minted cmd");
        assert!(matches!(
            e.history.0.last(),
            Some(Msg { role: Role::User, .. })
        ));
    }

    /// VC-1.7 (enqueue): a `UserMessage` arriving while the entity is `Thinking`
    /// PARKS on the per-entity Inbox — no Command is emitted, the in-flight turn's
    /// `Activity` is unchanged, the message is NOT folded into History, and it is
    /// durably queued on `inbox.pending` for the next turn.
    #[test]
    fn mid_run_user_message_enqueues_to_inbox_without_interrupting() {
        let world = thinking_world("starter", 7);
        let before = world.entities.get(&0).expect("entity").clone();

        let (world, commands) = tick(&world, &user_message("steer me", 2));

        assert!(commands.is_empty(), "a mid-run UserMessage emits no Command");
        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Thinking { cmd } if cmd == 7),
            "non-interrupt: the in-flight turn is untouched"
        );
        assert_eq!(
            e.history, before.history,
            "the steered message is NOT folded into the current turn's History"
        );
        assert_eq!(
            e.inbox.pending,
            vec![vec![Block::Text {
                text: "steer me".into()
            }]],
            "the steered message is durably parked on the Inbox"
        );
    }

    /// VC-1.7 (no-content-drop): two mid-run `UserMessage`s with IDENTICAL text BOTH
    /// land on `inbox.pending` (len == 2), proving that no content-equality heuristic
    /// silently drops a duplicate. The phase ordering (SteeringSystem before
    /// IntakeSystem) is the sole guard — every genuine mid-run message is enqueued
    /// regardless of its text content.
    #[test]
    fn identical_mid_run_messages_both_enqueue_no_content_drop() {
        let world = thinking_world("starter", 7);

        // First mid-run message with text "redo".
        let (world, c1) = tick(&world, &user_message("redo", 2));
        // Second mid-run message — IDENTICAL text.
        let (world, c2) = tick(&world, &user_message("redo", 3));

        assert!(c1.is_empty() && c2.is_empty(), "neither steer starts a turn");

        let e = world.entities.get(&0).expect("entity");
        assert_eq!(
            e.inbox.pending.len(),
            2,
            "both identical mid-run messages must be on the Inbox (no content-equality drop)"
        );
        assert_eq!(
            e.inbox.pending,
            vec![
                vec![Block::Text { text: "redo".into() }],
                vec![Block::Text { text: "redo".into() }],
            ],
            "both enqueued FIFO in arrival order"
        );
        // The in-flight turn is untouched.
        assert!(matches!(e.activity, Activity::Thinking { cmd } if cmd == 7));
        assert_eq!(
            e.history, thinking_world("starter", 7).entities.get(&0).expect("entity").history,
            "History is unchanged — neither message folded into the current turn"
        );
    }

    /// VC-1.7 (drain): two mid-run `UserMessage`s queue FIFO on the Inbox; the next
    /// turn initiation from `Idle` DRAINS them in arrival order, honours them as the
    /// turn's user input ahead of the triggering message, and leaves the Inbox empty.
    #[test]
    fn queued_inbox_messages_are_drained_fifo_and_honoured_on_next_initiation() {
        let world = thinking_world("starter", 7);

        // Two mid-run messages park on the Inbox in arrival order (no interrupt).
        let (world, c1) = tick(&world, &user_message("q1", 2));
        let (world, c2) = tick(&world, &user_message("q2", 3));
        assert!(c1.is_empty() && c2.is_empty(), "neither steer starts a turn");
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(
            e.inbox.pending,
            vec![
                vec![Block::Text { text: "q1".into() }],
                vec![Block::Text { text: "q2".into() }],
            ],
            "both steered messages queue FIFO (oldest first)"
        );
        assert!(matches!(e.activity, Activity::Thinking { cmd } if cmd == 7));

        // The in-flight turn ends → Idle; the queued messages remain parked.
        let (world, _) = tick(&world, &model_responded(7, "answer", 4));
        assert!(matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Idle
        ));

        // The next initiation DRAINS the Inbox FIFO into the turn, then empties it.
        let (world, commands) = tick(&world, &user_message("trigger", 5));
        assert_eq!(commands.len(), 1, "the next turn initiates with one CallModel");
        let e = world.entities.get(&0).expect("entity");
        assert!(e.inbox.pending.is_empty(), "the Inbox is emptied on drain");

        // The CallModel carries the user messages in FIFO order: the turn opener,
        // then the two queued steers, then the triggering message.
        let texts: Vec<String> = match &commands[0] {
            Command::CallModel { messages, .. } => messages
                .0
                .iter()
                .filter_map(|m| match (m.role, m.content.as_slice()) {
                    (Role::User, [Block::Text { text }]) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
            other => panic!("expected CallModel, got {other:?}"),
        };
        let expected: Vec<String> = ["starter", "q1", "q2", "trigger"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            texts, expected,
            "queued messages are honoured in FIFO order, ahead of the triggering message"
        );
    }

    /// VC-2.3 (Inbox DropOldest): a bounded enqueue past capacity evicts the OLDEST
    /// (front) entry, preserves FIFO, holds the post-push length at the cap, and emits
    /// a stratum-2 `MessageDropped` recording how many were dropped (Inv 11).
    #[test]
    fn inbox_overflow_drops_oldest_preserves_fifo_and_emits_message_dropped() {
        use crate::agent::world::lifecycle::{DropReason, LifecycleEvent};
        use crate::agent::world::world::{Inbox, WallClock};

        let mut inbox = Inbox {
            pending: vec![
                vec![Block::Text { text: "A".into() }],
                vec![Block::Text { text: "B".into() }],
            ],
        };
        let wall = WallClock { observed: Some(1_700_000_000) };

        let notice = super::bounded_enqueue(
            &mut inbox,
            vec![Block::Text { text: "C".into() }],
            2,
            wall,
        );

        // FIFO preserved, length held at the cap, oldest ("A") evicted.
        assert_eq!(
            inbox.pending,
            vec![
                vec![Block::Text { text: "B".into() }],
                vec![Block::Text { text: "C".into() }],
            ],
            "oldest evicted; the queue stays FIFO and bounded at the cap"
        );
        // The drop is observable: a stratum-2 MessageDropped recording one eviction.
        match notice {
            Some(LifecycleEvent::MessageDropped { reason, dropped, at, .. }) => {
                assert_eq!(reason, DropReason::InboxOverflow);
                assert_eq!(dropped, 1, "exactly one oldest entry was dropped");
                assert_eq!(at, wall, "stamped with the observed wall-time");
            }
            other => panic!("expected a MessageDropped notice, got {other:?}"),
        }
    }

    /// A `0` capacity is UNBOUNDED: the enqueue never evicts and emits no notice,
    /// however full the Inbox already is (the brake is simply not configured).
    #[test]
    fn inbox_capacity_zero_is_unbounded_no_drop() {
        use crate::agent::world::world::{Inbox, WallClock};
        let mut inbox = Inbox {
            pending: vec![
                vec![Block::Text { text: "A".into() }],
                vec![Block::Text { text: "B".into() }],
                vec![Block::Text { text: "C".into() }],
            ],
        };
        let notice = super::bounded_enqueue(
            &mut inbox,
            vec![Block::Text { text: "D".into() }],
            0,
            WallClock::default(),
        );
        assert!(notice.is_none(), "an unbounded (0) cap never drops");
        assert_eq!(inbox.pending.len(), 4, "every entry is retained when unbounded");
    }

    /// The bound is applied on the REAL tick path: a mid-run `UserMessage` steered onto
    /// a full, capped Inbox evicts the oldest so the queue stays FIFO and bounded — the
    /// brake is World state, so it replays deterministically (Inv 11).
    #[test]
    fn steering_enqueue_is_bounded_on_the_tick_path() {
        let mut world = thinking_world("starter", 7);
        {
            let e = world.entities.get_mut(&0).expect("entity");
            e.budget.limits.inbox_capacity = 2;
            e.inbox.pending = vec![
                vec![Block::Text { text: "q1".into() }],
                vec![Block::Text { text: "q2".into() }],
            ];
        }

        let (world, commands) = tick(&world, &user_message("q3", 2));

        // A mid-run steer starts NO turn — the only Command emitted is the
        // stratum-2 overflow notice, never a turn-starting `CallModel`.
        assert!(
            !commands.iter().any(|c| matches!(c, Command::CallModel { .. })),
            "a mid-run steer starts no turn"
        );
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(
            e.inbox.pending,
            vec![
                vec![Block::Text { text: "q2".into() }],
                vec![Block::Text { text: "q3".into() }],
            ],
            "the oldest (q1) is evicted; the Inbox stays FIFO and bounded at the cap"
        );
    }

    /// VC-2.3 (driver path): an Inbox overflow on the LIVE tick/driver path surfaces
    /// the stratum-2 `MessageDropped { InboxOverflow }` notice to the event log via
    /// the driver's `append_lifecycle` — the notice is NO LONGER discarded. The
    /// eviction itself stays in the pure fold (World state), so replay reconstructs
    /// the bounded Inbox WITHOUT this stratum-2 record (it is live-only, neutral on
    /// replay). The steering overflow emits no model call, so the client is never
    /// reached. (Inv 11.)
    #[tokio::test]
    async fn inbox_overflow_emits_message_dropped_on_the_live_driver_path() {
        use crate::agent::world::effects::{
            drive_live, ResultStamp, SurfaceDriver, UnattachedPeerSender,
        };
        use crate::agent::world::event_log::{EventLog, MemoryEventLog};
        use crate::agent::world::lifecycle::{DropReason, LifecycleEvent};
        use crate::error::Error;
        use crate::provider::{ModelApi, ModelInfo, ModelRequest, ModelResponse};
        use std::future::Future;
        use std::pin::Pin;

        /// A model client that must never be invoked: a steering overflow emits ONLY
        /// the stratum-2 notice Command, never a `CallModel`. A call here would prove
        /// the notice path wrongly triggered an effect.
        struct NoCallClient;
        impl ModelApi for NoCallClient {
            fn turn<'a>(
                &'a self,
                _req: &'a ModelRequest,
            ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
                Box::pin(async { panic!("a steering overflow must not invoke the model client") })
            }
            fn list_models<'a>(
                &'a self,
            ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn model(&self) -> &str {
                "stub"
            }
        }

        /// A no-op surface-drive sink: this overflow test emits no `set_value`
        /// RunTool, so the driver is never reached — it stands in for the injected
        /// sink so `drive_live` type-checks.
        struct NoSurfaceDrive;
        impl SurfaceDriver for NoSurfaceDrive {
            async fn drive(&self, _command: &Command) -> Result<(), Error> {
                Ok(())
            }
        }

        // A mid-run entity whose capped Inbox is already at capacity.
        let mut world = thinking_world("starter", 7);
        {
            let e = world.entities.get_mut(&0).expect("entity");
            e.budget.limits.inbox_capacity = 2;
            e.inbox.pending = vec![
                vec![Block::Text { text: "q1".into() }],
                vec![Block::Text { text: "q2".into() }],
            ];
        }

        // The mid-run steer overflows the cap → tick emits the stratum-2 notice
        // Command (and the eviction is already folded into the World).
        let (world, commands) = tick(&world, &user_message("q3", 2));
        assert!(
            !commands.is_empty(),
            "an overflowing steer emits the stratum-2 notice Command"
        );
        let e = world.entities.get(&0).expect("entity");
        assert_eq!(
            e.inbox.pending,
            vec![
                vec![Block::Text { text: "q2".into() }],
                vec![Block::Text { text: "q3".into() }],
            ],
            "the eviction is in the stratum-1 fold: oldest dropped, FIFO preserved"
        );

        // The LIVE driver appends the notice to the stratum-2 log via append_lifecycle.
        let mut log = MemoryEventLog::new();
        let stamp = ResultStamp {
            edge: 0,
            app_edge: 1,
            at: 3,
            wall: None,
        };
        let results = drive_live(
            &commands,
            stamp,
            &world,
            &NoCallClient,
            &NoSurfaceDrive,
            &UnattachedPeerSender,
            &mut log,
        )
        .await
        .expect("drive_live");
        assert!(
            results.is_empty(),
            "the notice produces no stratum-1 result Event (it is stratum-2 only)"
        );

        let lifecycle = log.load_lifecycle().expect("load lifecycle");
        assert!(
            lifecycle.iter().any(|ev| matches!(
                ev,
                LifecycleEvent::MessageDropped { reason: DropReason::InboxOverflow, dropped, .. }
                    if *dropped >= 1
            )),
            "the Inbox overflow notice reaches the stratum-2 log via append_lifecycle"
        );
    }
}
