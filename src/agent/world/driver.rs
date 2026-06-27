//! driver — the imperative-shell tick-driver loop. See docs/agent/world/ecs-runtime.md
//!
//! [`WorldDriver`] is the IMPERATIVE SHELL that drives the pure World runtime
//! live: it owns the IO capabilities (a [`ModelCaller`], a [`SurfaceDriver`] and
//! an [`EventLog`]) the pure `tick`/Systems are forbidden to touch, and turns a
//! stream of [`LogicalInput`]s into recorded `Event`s + a settled `World`.
//!
//! It is the live counterpart to the offline replay loop in
//! `tests/integration/agent/world_walking_skeleton.rs`, promoted out of the test
//! into a reusable, app-worker-driving module. For each submitted input it:
//!
//!   1. wraps the input in an `Event { origin, edge, at, wall, input }`,
//!   2. APPENDS that Event to the log BEFORE folding it (log-before-apply, Inv 4),
//!   3. folds it through the pure `tick` reducer to get the next `World` + the
//!      tick's emitted `Command`s,
//!   4. hands the Commands to the LIVE driver `drive_live` — which performs the
//!      real effect (model call / surface drive), appends each result `Event`,
//!      and returns it — then folds every result back through `tick`, repeating
//!      until the entity is quiescent (the tool-loop continuation `CallModel`
//!      results re-enter `tick` here), and
//!   5. advances the logical `Tick` monotonically across every appended Event
//!      (deterministic ticks, Inv 8/12).
//!
//! After each input settles it DERIVES `EntryNotification`s from the primary
//! entity's `History` delta (the assistant text the turn produced) so a host/app
//! can still render the agent's reply, exactly as the legacy `AgentLoop`'s
//! `OutputPort` did — without re-introducing the legacy loop.
//!
//! The driver is GENERIC over the three IO traits so it is OFFLINE-TESTABLE: a
//! test injects a `MemoryEventLog`, a stub `ModelCaller`, and a no-op
//! `SurfaceDriver` and drives a full `Idle → Thinking → Idle` turn with no
//! network. The functional core (`tick`, the Systems, `drive_live`) stays pure;
//! this loop is the only place clocks, the network, and the log are touched.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::entry::{Entry, EntryOrigin};
use crate::agent::runtime::port::EntryNotification;
use crate::provider::moonshot::Message;
use crate::error::Error;

use super::effects::{drive_live, ModelCaller, ResultStamp, SurfaceDriver};
use super::event_log::EventLog;
use super::gates::EntityGate;
use super::history::{Block, History, Msg, Role};
use super::inputs::{Event, LogicalInput, Origin};
use super::world::{
    Activity, Components, EdgeId, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources, Tick,
    Timestamp, World,
};

/// The primary entity's id. The shell SEEDS this `Idle` entity at startup because
/// no P0 System creates an entity from `SessionStarted` (IntakeSystem only ADMITS
/// a `UserMessage`, and it NO-OPS when its addressed entity is absent or not
/// `Idle`), so genesis is the shell's responsibility. See
/// docs/agent/world/ecs-runtime.md (Genesis; IntakeSystem).
const PRIMARY_ENTITY: EntityId = 0;

/// The single counterpart relationship the P0 driver routes on: the human edge.
/// Host `UserMessage`s and CONVERSATION results (the model call) are stamped onto
/// it deterministically.
const HUMAN_EDGE: EdgeId = 0;

/// The DISTINCT app/surface edge the agent's SURFACE-WRITE results (the `set_value`
/// `ToolReturned`) are stamped onto, kept separate from `HUMAN_EDGE` so the per-edge
/// mode projection (Inv 19) folds the agent's surface drive as `Driven` (agent-only
/// on this edge) while the human conversation edge stays `Assisted` — the design's
/// concurrent per-edge modes. See docs/agent/world/ecs-runtime.md (Mode-as-projection).
const APP_EDGE: EdgeId = 1;

/// The surface-manipulation tools the live whiteboard App offers its surface-capable
/// ROOT entity, so a live model call is told `set_value` EXISTS and can request it
/// (GAP A). The SOLE source of these names: `bootstrap` RECORDS them into the
/// `SessionStarted` header, and the `SessionStarted` fold (SurfaceSystem) seeds them
/// into `Resources.surface_tools`, which the CallModel-emitting Systems read into
/// each turn's `ToolSet`. Recording-then-folding (rather than seeding the live World
/// directly) is what makes a replay reconstruct surface tools identically (Inv 6/7).
/// The milestone's App is the whiteboard, whose sole surface tool is `set_value`
/// (YAGNI — no general tool registry). See docs/agent/world/ecs-runtime.md (Anthropic
/// model-call mapping; SurfaceSystem).
fn surface_tool_names() -> super::effects::ToolSet {
    super::effects::ToolSet(vec!["set_value".to_string()])
}

/// The imperative-shell loop that drives the pure World live.
///
/// Generic over the three IO capabilities so it is offline-testable:
/// - `C: ModelCaller` — the model-call effect (`MessagesClient` in production),
/// - `S: SurfaceDriver` — the surface-drive sink (the app-worker forwarder), and
/// - `L: EventLog` — the append-only log (`FilesystemEventLog` in production).
pub struct WorldDriver<C, S, L> {
    /// The current settled World, rebuilt by folding every appended Event.
    world: World,
    /// The model-call capability the LIVE driver dispatches `CallModel` through.
    client: C,
    /// The surface-drive sink the LIVE driver forwards `set_value` `RunTool`s to.
    surface_driver: S,
    /// The append-only event log; the sole source of truth (Inv 9).
    log: L,
    /// The monotonic logical tick the NEXT appended Event is stamped at. Advanced
    /// once per appended Event so ticks never depend on scheduling (Inv 8/12).
    next_at: Tick,
    /// How many of the primary entity's `History` messages have already been
    /// emitted as `EntryNotification`s; the watermark for the history-delta diff.
    history_emitted: usize,
}

impl<C, S, L> WorldDriver<C, S, L>
where
    C: ModelCaller,
    S: SurfaceDriver,
    L: EventLog,
{
    /// Bootstrap a fresh-session driver: build the genesis `World` (a single
    /// primary `Idle` entity seeded from `seed`, with `model` as the world-default
    /// config), record the `SessionStarted` header at tick 0 — carrying BOTH the
    /// RNG `seed` AND the App's `surface_tools`, the two hidden inputs that must
    /// cross a recorded boundary (Inv 9) — and fold it. The fold (SurfaceSystem)
    /// is what seeds `Resources.surface_tools`; bootstrap RECORDS the tools but no
    /// longer seeds the World directly, keeping ONE source of truth so the live
    /// World and a replay reconstruct surface tools identically. The driver is then
    /// ready to accept inputs starting at tick 1.
    pub fn bootstrap(
        seed: u64,
        model: ModelConfig,
        client: C,
        surface_driver: S,
        mut log: L,
    ) -> Result<Self, Error> {
        let mut world = World::new(PRIMARY_ENTITY, Resources::new(seed, model));
        world
            .entities
            .insert(PRIMARY_ENTITY, primary_idle_components());

        // The session header carries the hidden inputs that must cross a recorded
        // boundary: the RNG `seed` and the App's `surface_tools` (the whiteboard's
        // `set_value`). Recording the tools here — and letting the fold seed
        // `Resources.surface_tools` — is the SINGLE source of truth (GAP A). Record
        // at tick 0 BEFORE folding (Inv 4/9). Offline genesis (the replay tests)
        // records empty tools, so their requests stay byte-identical to a pre-tools call.
        let session = Event {
            origin: Origin::System,
            edge: HUMAN_EDGE,
            at: 0,
            wall: now_wall(),
            input: LogicalInput::SessionStarted {
                seed,
                surface_tools: surface_tool_names().0,
            },
        };
        log.append(&session)?;
        // Folding SessionStarted seeds `Resources.surface_tools` from the recorded
        // names (SurfaceSystem) and advances the logical clock.
        let (world, _no_commands) = super::systems::tick(&world, &session);

        Ok(Self {
            world,
            client,
            surface_driver,
            log,
            next_at: 1,
            history_emitted: 0,
        })
    }

    /// Submit one logical input, drive the World to quiescence, and return the
    /// `EntryNotification`s the turn produced.
    ///
    /// Wraps the input in its `Event` envelope, APPENDS it (log-before-apply,
    /// Inv 4), folds it through `tick`, then drains the emitted Commands through
    /// the LIVE driver — re-folding each result `Event` until no Commands remain
    /// (the tool-loop continuation `CallModel` results re-enter `tick` here).
    /// Finally it derives the new assistant `History` as `EntryNotification`s.
    pub async fn submit(&mut self, input: LogicalInput) -> Result<Vec<EntryNotification>, Error> {
        let event = Event {
            origin: origin_for(&input),
            edge: HUMAN_EDGE,
            at: self.next_at,
            wall: now_wall(),
            input,
        };
        self.next_at += 1;

        // Log-before-apply (Inv 4): durable BEFORE the reducer folds it.
        self.log.append(&event)?;
        let (next, commands) = super::systems::tick(&self.world, &event);
        self.world = next;

        self.drive_to_quiescence(commands).await?;
        Ok(self.derive_entry_notifications())
    }

    /// Drive a tick's emitted Commands to quiescence: dispatch each batch through
    /// `drive_live` (real effect + log-before-apply), fold every result back
    /// through `tick`, and repeat while the fold keeps emitting Commands. Each
    /// `drive_live` call drives ONE tick's worth of effects and stamps its results
    /// at a fresh monotonic tick.
    async fn drive_to_quiescence(&mut self, mut commands: Vec<crate::agent::world::effects::Command>) -> Result<(), Error> {
        while !commands.is_empty() {
            let stamp = ResultStamp {
                edge: HUMAN_EDGE,
                // Agent surface-write results route to the DISTINCT app edge so
                // `mode(APP_EDGE)` folds Driven, independent of the human edge (GAP B).
                app_edge: APP_EDGE,
                at: self.next_at,
                wall: now_wall(),
            };
            self.next_at += 1;

            // The LIVE driver performs the real effect and appends each result
            // Event to the log BEFORE returning it (log-before-apply, Inv 4). The
            // surfaces it fingerprints over are the World's perceived view, so a
            // re-emitted UI request diverges exactly when that surface changed
            // (Inv 18 / Theme 2b).
            let results = drive_live(
                &commands,
                stamp,
                &self.world.resources.surfaces,
                &self.client,
                &self.surface_driver,
                &mut self.log,
            )
            .await?;

            commands = Vec::new();
            for result in &results {
                let (next, mut cmds) = super::systems::tick(&self.world, result);
                self.world = next;
                commands.append(&mut cmds);
            }
        }
        Ok(())
    }

    /// Derive `EntryNotification`s from the primary entity's `History` delta since
    /// the last call: each NEW assistant message carrying text becomes one
    /// `EntryNotification` so a host/app renders the agent's reply, mirroring the
    /// legacy `AgentLoop`'s `OutputPort`. The entry `id` is the message's stable
    /// position in `History`; `parent_id` is its predecessor. Messages without
    /// assistant text (pure tool_use turns, user turns the app already shows) are
    /// not emitted. The watermark advances past every inspected message so each is
    /// emitted at most once.
    fn derive_entry_notifications(&mut self) -> Vec<EntryNotification> {
        let mut out = Vec::new();
        let Some(entity) = self.world.entities.get(&PRIMARY_ENTITY) else {
            return out;
        };
        let messages = &entity.history.0;
        for index in self.history_emitted..messages.len() {
            let message = &messages[index];
            if !matches!(message.role, Role::Assistant) {
                continue;
            }
            let Some(text) = assistant_text(message) else {
                continue;
            };
            out.push(EntryNotification {
                entry: Entry {
                    id: index,
                    parent_id: index.checked_sub(1),
                    message: Message::Assistant {
                        content: Some(text),
                        reasoning_content: None,
                        tool_calls: None,
                        partial: None,
                    },
                    origin: EntryOrigin::Assistant,
                    channel_metadata: None,
                },
                // The turn has settled (quiescent) by the time this runs, so the
                // derived assistant text is final.
                is_final: true,
            });
        }
        self.history_emitted = messages.len();
        out
    }
}

/// The genesis primary entity: `Identity::Primary`, root lineage, empty History,
/// `Idle`. Mirrors the `idle_world()` fixtures so the live World starts exactly
/// where the Systems' unit tests assume. See docs/agent/world/ecs-runtime.md.
fn primary_idle_components() -> Components {
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
        inbox: Inbox::default(),
        turns: 0,
        model: None,
    }
}

/// The `Event.origin` a submitted input is stamped with. A host `UserMessage` and
/// a human surface mutation are `Human`-origin free variables (Inv 18); a surface
/// observation is a `System` perception. Everything else is `System`-neutral.
fn origin_for(input: &LogicalInput) -> Origin {
    match input {
        LogicalInput::UserMessage { .. } | LogicalInput::SurfaceMutated { .. } => Origin::Human,
        _ => Origin::System,
    }
}

/// Concatenate an assistant message's `Text` blocks (newline-joined), returning
/// `None` when the message carries no text (e.g. a pure tool_use turn).
fn assistant_text(message: &Msg) -> Option<String> {
    let mut text = String::new();
    for block in &message.content {
        if let Block::Text { text: chunk } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(chunk);
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The wall-time the shell observes when it appends an Event — the recorded
/// hidden-input boundary the agent reads as data (Inv 2). The PURE core never
/// calls a clock; the shell records it here so replay reproduces it from the log.
/// A clock before the epoch (unreachable in practice) records `None`.
fn now_wall() -> Option<Timestamp> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as Timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::effects::Command;
    use crate::agent::world::event_log::MemoryEventLog;
    use crate::agent::world::inputs::{
        Capabilities, ModelMeta, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::world::Effort;
    use serde_json::Value as Json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `ModelCaller` that returns a fixed `EndTurn` assistant text and counts its
    /// invocations, so a test proves EXACTLY ONE model call happened (one
    /// `Idle → Thinking → Idle` turn) without a network.
    struct StubModelCaller {
        text: String,
        calls: AtomicUsize,
    }

    impl ModelCaller for StubModelCaller {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok((
                vec![Block::Text {
                    text: self.text.clone(),
                }],
                ModelMeta {
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    },
                    model_id: "claude-stub".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            ))
        }
    }

    /// A `SurfaceDriver` that fails loudly if reached: a model-only turn never
    /// drives a surface, so a `CallModel`-only path proves the sink is untouched.
    struct NoSurfaceDrive;

    impl SurfaceDriver for NoSurfaceDrive {
        async fn drive(&self, _command: &Command) -> Result<(), Error> {
            panic!("a CallModel-only turn must not reach the surface driver");
        }
    }

    fn stub_model() -> ModelConfig {
        ModelConfig {
            model: "claude-stub".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// The acceptance self-check: a `UserMessage` drives the primary entity
    /// `Idle → Thinking → Idle`, the log records `SessionStarted` + `UserMessage` +
    /// `ModelResponded` in order, and an `EntryNotification` carrying the assistant
    /// text is emitted. Offline: `MemoryEventLog` + stub `ModelCaller` + no-op
    /// `SurfaceDriver`, no network.
    #[tokio::test]
    async fn user_message_drives_idle_to_thinking_to_idle_logs_and_emits_entry() {
        let assistant_text = "pong";
        let client = StubModelCaller {
            text: assistant_text.into(),
            calls: AtomicUsize::new(0),
        };

        let mut driver =
            WorldDriver::bootstrap(7, stub_model(), client, NoSurfaceDrive, MemoryEventLog::new())
                .expect("bootstrap the driver");

        // Pre-condition: the seeded primary entity starts Idle.
        assert!(
            matches!(
                driver.world.entities.get(&PRIMARY_ENTITY).unwrap().activity,
                Activity::Idle
            ),
            "the seeded primary entity starts Idle"
        );

        // bootstrap RECORDS the App's surface tools into SessionStarted and the
        // fold (SurfaceSystem) seeds `Resources.surface_tools` — bootstrap never
        // seeds the World directly, yet the live World still carries `set_value`.
        assert_eq!(
            driver.world.resources.surface_tools,
            crate::agent::world::effects::ToolSet(vec!["set_value".into()]),
            "the SessionStarted fold reconstructs Resources.surface_tools (single source of truth)"
        );
        assert!(
            matches!(
                driver.log.load().expect("load log").first().map(|e| &e.input),
                Some(LogicalInput::SessionStarted { surface_tools, .. })
                    if surface_tools == &vec!["set_value".to_string()]
            ),
            "the recorded SessionStarted header carries the App's surface tools"
        );

        let notifications = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "ping".into(),
            })
            .await
            .expect("the turn drives to quiescence");

        // The turn settled back to Idle: Idle → Thinking (Intake's CallModel) →
        // Idle (EndTurn). Exactly ONE model call proves the single Thinking turn.
        let entity = driver.world.entities.get(&PRIMARY_ENTITY).unwrap();
        assert!(
            matches!(entity.activity, Activity::Idle),
            "the turn returns Thinking -> Idle"
        );
        assert_eq!(
            driver.client.calls.load(Ordering::SeqCst),
            1,
            "exactly one model call (one Idle->Thinking->Idle turn)"
        );

        // History holds the user turn then the assistant reply.
        assert_eq!(entity.history.0.len(), 2, "History holds user + assistant");
        assert!(matches!(
            &entity.history.0[1],
            Msg { role: Role::Assistant, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == assistant_text)
        ));

        // The log records SessionStarted, then UserMessage, then ModelResponded.
        let events = driver.log.load().expect("load the recorded log");
        assert_eq!(events.len(), 3, "SessionStarted + UserMessage + ModelResponded");
        assert!(matches!(events[0].input, LogicalInput::SessionStarted { .. }));
        assert!(matches!(events[1].input, LogicalInput::UserMessage { .. }));
        assert!(matches!(events[2].input, LogicalInput::ModelResponded { .. }));

        // Ticks advance monotonically across every appended Event (Inv 8/12).
        assert_eq!(events[0].at, 0);
        assert_eq!(events[1].at, 1);
        assert_eq!(events[2].at, 2);

        // An EntryNotification carrying the assistant text was derived from the
        // History delta so the app can render the agent's reply.
        assert_eq!(notifications.len(), 1, "one assistant EntryNotification");
        let note = &notifications[0];
        assert!(note.is_final, "a settled turn's assistant text is final");
        assert_eq!(note.entry.origin, EntryOrigin::Assistant);
        assert_eq!(
            note.entry.message.content_text(),
            assistant_text,
            "the notification carries the assistant text"
        );
    }

    /// A second submitted input only emits the NEW assistant text — the
    /// history-delta watermark suppresses re-emitting earlier turns.
    #[tokio::test]
    async fn second_turn_emits_only_the_new_assistant_text() {
        let client = StubModelCaller {
            text: "first".into(),
            calls: AtomicUsize::new(0),
        };
        let mut driver =
            WorldDriver::bootstrap(7, stub_model(), client, NoSurfaceDrive, MemoryEventLog::new())
                .expect("bootstrap");

        let first = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "one".into(),
            })
            .await
            .expect("first turn");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].entry.message.content_text(), "first");

        let second = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "two".into(),
            })
            .await
            .expect("second turn");
        // Only the second turn's assistant text — never a re-emit of the first.
        assert_eq!(second.len(), 1, "only the new assistant message is emitted");
        assert_eq!(second[0].entry.message.content_text(), "first");
        assert_eq!(
            second[0].entry.id, 3,
            "the entry id is the new message's History position (user0, asst1, user2, asst3)"
        );
    }

    /// A `ModelCaller` that drives a tool turn then ends: it returns a `set_value`
    /// tool_use on its FIRST call (so the agent performs a surface write) and an
    /// `EndTurn` text reply on its second (so the turn settles). Counts its calls so
    /// the test proves exactly two model calls bracket the one surface write.
    struct ToolThenEndCaller {
        calls: AtomicUsize,
    }

    impl ModelCaller for ToolThenEndCaller {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let (blocks, stop_reason) = if n == 0 {
                (
                    vec![Block::ToolUse {
                        id: "tu_set_value".into(),
                        name: "set_value".into(),
                        input: serde_json::json!({
                            "surface_ops": [
                                { "op": "set_value", "surface": 9, "element": 2, "value": "agent typed", "base_version": null }
                            ]
                        }),
                    }],
                    StopReason::ToolUse,
                )
            } else {
                (vec![Block::Text { text: "done".into() }], StopReason::EndTurn)
            };
            Ok((
                blocks,
                ModelMeta {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    model_id: "claude-stub".into(),
                    stop_reason,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            ))
        }
    }

    /// A `SurfaceDriver` that ACCEPTS every drive (the macOS-app stand-in succeeds),
    /// so the live `set_value` executor records its `ToolReturned` without a real app.
    struct AcceptingSurfaceDriver;

    impl SurfaceDriver for AcceptingSurfaceDriver {
        async fn drive(&self, _command: &Command) -> Result<(), Error> {
            Ok(())
        }
    }

    /// (B) GAP B: when the live driver executes an agent `set_value` surface write, it
    /// stamps that `ToolReturned` on the DISTINCT app edge — so `mode(APP_EDGE)` folds
    /// `Driven` (agent-only on that edge) while the human conversation edge stays
    /// `Assisted` (the Human `UserMessage` + the Agent model replies), realizing the
    /// design's concurrent per-edge modes (Inv 19). Offline: stub model + accepting
    /// surface driver + `MemoryEventLog`, no network and no macOS app.
    #[tokio::test]
    async fn agent_surface_write_folds_driven_on_the_app_edge_human_edge_unaffected() {
        use crate::agent::world::mode::{mode, Mode};

        let mut driver = WorldDriver::bootstrap(
            7,
            stub_model(),
            ToolThenEndCaller {
                calls: AtomicUsize::new(0),
            },
            AcceptingSurfaceDriver,
            MemoryEventLog::new(),
        )
        .expect("bootstrap the driver");

        driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "type into the field".into(),
            })
            .await
            .expect("the surface-driving turn drives to quiescence");

        // Exactly two model calls bracket the one surface write (ToolUse then EndTurn).
        assert_eq!(
            driver.client.calls.load(Ordering::SeqCst),
            2,
            "two model calls (ToolUse → set_value → EndTurn) bracket the surface write"
        );

        let events = driver.log.load().expect("load the recorded log");

        // The agent's surface write is recorded on the APP edge — exactly one
        // agent-origin `ToolReturned` there (the agent UI write's sole fact, Inv 18).
        let app_writes: Vec<&Event> = events
            .iter()
            .filter(|e| e.edge == APP_EDGE && matches!(e.input, LogicalInput::ToolReturned { .. }))
            .collect();
        assert_eq!(app_writes.len(), 1, "the set_value write lands on the app edge");
        assert_eq!(app_writes[0].origin, Origin::Agent, "the write is agent-origin");

        // No surface write contaminates the human conversation edge.
        assert!(
            !events.iter().any(
                |e| e.edge == HUMAN_EDGE && matches!(e.input, LogicalInput::ToolReturned { .. })
            ),
            "no surface write is stamped on the human conversation edge (GAP B)"
        );

        // mode(APP_EDGE) folds Driven (agent-only); the human edge is NOT Driven —
        // the Human `UserMessage` keeps it Assisted (concurrent per-edge modes).
        let window = 16;
        assert_eq!(
            mode(&events, APP_EDGE, window),
            Mode::Driven,
            "the agent-only app edge folds Driven (GAP B / Inv 19)"
        );
        assert_eq!(
            mode(&events, HUMAN_EDGE, window),
            Mode::Assisted,
            "the human conversation edge stays Assisted (Human + Agent), never Driven"
        );
    }
}
