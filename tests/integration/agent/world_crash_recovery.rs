//! VC-2.1 / VC-2.2 — the SAFETY integration sink for WRITE-AHEAD ordering and
//! CRASH RECOVERY, composed over the same `src/agent/world/` stack the walking
//! skeleton wires (genesis, the pure `tick` reducer, the LIVE driver `drive_live`,
//! and the resume/recovery path `resume`). It touches no `src/agent/world/*` file.
//!
//! - **VC-2.1 (WAL ordering + ctx inheritance)** — driving one live `CallModel`
//!   through `drive_live` write-aheads the stratum-2 `CommandDispatched`
//!   dispatch-intent BEFORE the stratum-1 result Event (intent before commitment,
//!   Inv 5), and the result Event INHERITS its `entity`/`origin`/`edge` from the
//!   dispatch `ctx` (Inv 17) rather than from a positional scan.
//! - **VC-2.2 (crash recovery)** — a stratum-1 log truncated AFTER a
//!   `CommandDispatched { CallModel }` but BEFORE its result (crash mid-`Thinking`)
//!   resumes: `resume` SYNTHESISES and LOGS a stratum-1 `InferenceCancelled { Crash }`
//!   filling `cmd`/`entity`/`fingerprint` from the dangling dispatch, the reducer
//!   folds it and the entity SETTLES `Thinking → Idle` (never a stuck sink, Inv 10).
//!   A fresh ACCOUNTED retry (new `cmd`, new `key`) is autonomy/budget's job — named
//!   here, not performed.
//!
//! Both logs are hand-authored synthetic; neither makes a live model call, so the
//! file needs no credentials and runs on every developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 5 (intent before commitment), 6
//! (replay determinism), 10 (totality — no stuck state), 17 (cmd → ctx inheritance).

use serde_json::Value as Json;

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{
    Command, CommandKey, ModelCaller, Reconciliation, ResultStamp, SurfaceDriver, ToolSet,
    UnattachedPeerSender, drive_live, fingerprint_call, resume,
};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::inputs::{
    CancelReason, Capabilities, Event, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::lifecycle::{
    ActorCtx, AppId, EffectKind, IdempotencyKey, LifecycleEvent,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8). Inert here — no System
// draws — but it must reseed identically for the rebuilt tail to match.
const SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Genesis — the World construction no System performs from `SessionStarted`
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary entity at
/// `Idle`, and `Resources` seeded from the session's `seed`. Mirrors the
/// walking-skeleton genesis the rest of the harness depends on.
fn genesis(seed: u64, model: &ModelConfig) -> World {
    let mut world = World::new(0, Resources::new(seed, model.clone()));
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
        },
    );
    world
}

/// The world-default `ModelConfig`. Its `model` id rides in the request the
/// `CallModel` fingerprints, so a synthesised result is stamped with a fingerprint
/// computed against THIS exact value; it makes no real call.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-crash-recovery".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// A trivial World whose perceived surface view is empty — the `&World` a
/// CallModel-only `drive_live` reads only `resources.surfaces` (empty) from.
fn surfaceless_world() -> World {
    World::new(0, Resources::new(7, offline_model()))
}

fn session_started(seed: u64) -> Event {
    Event {
        origin: Origin::System,
        edge: 0,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed,
            surface_tools: Vec::new(),
        },
    }
}

fn user_message(at: u64, text: &str) -> Event {
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

// ---------------------------------------------------------------------------
// Model-call stand-ins — the LIVE driver's `ModelCaller` capability
// ---------------------------------------------------------------------------

/// A `ModelCaller` returning a fixed successful response, so `drive_live` produces a
/// `ModelResponded` without a network. Mirrors the effects.rs unit-test stub.
struct StubClient;

impl ModelCaller for StubClient {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        Ok((
            vec![Block::Text { text: "ok".into() }],
            ModelMeta {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                model_id: "claude-crash-recovery".into(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        ))
    }
}

/// A no-op surface-drive sink: these crash-recovery tests drive only `CallModel`
/// (no `set_value` RunTool), so the driver is never reached — it stands in for
/// the injected sink so `drive_live` type-checks.
struct NoSurfaceDrive;
impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// OrderedLog — an interleave-recording `EventLog` that witnesses WAL ordering
// ---------------------------------------------------------------------------

/// One append against the log, tagged by stratum, preserving INTERLEAVED order.
#[derive(Clone)]
enum Appended {
    Stratum1(Event),
    Stratum2(LifecycleEvent),
}

/// An `EventLog` that records the interleaved append order across BOTH strata.
///
/// `MemoryEventLog` keeps stratum 1 and stratum 2 in SEPARATE vectors (mirroring the
/// filesystem log's parallel-stream layout), so it cannot witness the cross-stratum
/// ORDERING that Inv 5 ("intent before commitment") asserts. This recording log — the
/// integration-level mirror of the effects.rs unit-test `OrderedLog` — captures the
/// single interleaved sequence so the dispatch-intent-BEFORE-result ordering is a
/// genuine runtime assertion. It is a faithful `EventLog`: `load`/`load_lifecycle`
/// project the two strata back out, exactly like `MemoryEventLog`.
#[derive(Default)]
struct OrderedLog {
    appended: Vec<Appended>,
}

impl EventLog for OrderedLog {
    fn append(&mut self, event: &Event) -> Result<(), Error> {
        self.appended.push(Appended::Stratum1(event.clone()));
        Ok(())
    }
    fn load(&self) -> Result<Vec<Event>, Error> {
        Ok(self
            .appended
            .iter()
            .filter_map(|a| match a {
                Appended::Stratum1(e) => Some(e.clone()),
                Appended::Stratum2(_) => None,
            })
            .collect())
    }
    fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error> {
        self.appended.push(Appended::Stratum2(event.clone()));
        Ok(())
    }
    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
        Ok(self
            .appended
            .iter()
            .filter_map(|a| match a {
                Appended::Stratum2(e) => Some(e.clone()),
                Appended::Stratum1(_) => None,
            })
            .collect())
    }
}

/// One `CallModel` for entity 0, carrying `text` as its sole user message — the unit
/// of work the live driver write-aheads then dispatches.
fn call_model(cmd: CmdId, text: &str) -> Command {
    Command::CallModel {
        cmd,
        entity: 0,
        messages: History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text { text: text.into() }],
        }]),
        tools: ToolSet::default(),
        params: offline_model(),
        key: CommandKey,
    }
}

// ---------------------------------------------------------------------------
// VC-2.1 — write-ahead dispatch-intent precedes the result; result inherits ctx
// ---------------------------------------------------------------------------

/// **VC-2.1** Driving one live `CallModel` through `drive_live` (1) appends the
/// stratum-2 `CommandDispatched` dispatch-intent BEFORE the stratum-1 result Event
/// (intent before commitment, Inv 5) and (2) produces a result Event whose
/// `entity`/`origin`/`edge` and `fingerprint` INHERIT from the dispatch `ctx`
/// (Inv 17). The interleave-recording `OrderedLog` witnesses the ordering; a plain
/// `MemoryEventLog` (the production-shaped log) confirms the same ctx inheritance is
/// retrievable across its two parallel streams.
#[tokio::test]
async fn vc_2_1_wal_dispatch_intent_precedes_result_and_result_inherits_ctx() {
    let command = call_model(3, "hi");
    let stamp = ResultStamp {
        edge: 9,
        app_edge: 1,
        at: 5,
        wall: Some(1_700_000_000),
    };

    // --- Ordering (Inv 5) on an interleave-recording log ----------------------
    let mut ordered = OrderedLog::default();
    let results = drive_live(
        &[command.clone()],
        stamp,
        &surfaceless_world(),
        &StubClient,
        &NoSurfaceDrive,
        &UnattachedPeerSender,
        &mut ordered,
    )
    .await
    .expect("the live driver dispatches the CallModel");

    assert_eq!(
        ordered.appended.len(),
        2,
        "one dispatch-intent and one result for a single CallModel"
    );
    assert!(
        matches!(
            ordered.appended[0],
            Appended::Stratum2(LifecycleEvent::CommandDispatched { .. })
        ),
        "the stratum-2 CommandDispatched is write-ahead'd FIRST (intent before commitment, Inv 5)"
    );
    assert!(
        matches!(ordered.appended[1], Appended::Stratum1(_)),
        "the stratum-1 result Event is appended AFTER the dispatch-intent (log-before-apply, Inv 4)"
    );

    // The dispatch-intent carries ctx = (entity, origin = Agent, edge), the
    // idempotency key = (app_id, tick, effect_id), and the request fingerprint.
    let dispatched = ordered.load_lifecycle().expect("load stratum-2");
    let LifecycleEvent::CommandDispatched {
        cmd,
        at,
        kind,
        ctx,
        key,
        fingerprint,
    } = &dispatched[0]
    else {
        panic!("expected a CommandDispatched dispatch-intent");
    };
    assert_eq!(*cmd, 3);
    assert_eq!(*at, 5);
    assert_eq!(*kind, EffectKind::CallModel);
    assert_eq!(ctx.entity, 0);
    assert_eq!(ctx.origin, Origin::Agent);
    assert_eq!(ctx.edge, 9, "ctx.edge is the result's counterpart edge (the stamp edge)");
    assert_eq!(key.tick, 5);
    assert_eq!(key.effect_id, 0);

    // ctx inheritance (Inv 17): the result Event's envelope and the inner result
    // inherit entity/origin/edge/fingerprint from the dispatch-intent — NOT a
    // positional scan.
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result.origin, ctx.origin, "result origin inherited from ctx");
    assert_eq!(result.edge, ctx.edge, "result edge inherited from ctx");
    match &result.input {
        LogicalInput::ModelResponded {
            entity,
            cmd: rcmd,
            fingerprint: rfp,
            ..
        } => {
            assert_eq!(*entity, ctx.entity, "result entity inherited from ctx");
            assert_eq!(*rcmd, 3, "result correlates back to the dispatched cmd");
            assert_eq!(rfp, fingerprint, "result fingerprint binds back to the intent");
        }
        other => panic!("expected ModelResponded, got {other:?}"),
    }

    // --- The same against a production-shaped MemoryEventLog -------------------
    // Its parallel streams cannot witness interleaving (the OrderedLog above did),
    // but `load_lifecycle` carries the dispatch-intent and `load` the result, both
    // bound by the inherited ctx — proving the ctx index is durable on the real log.
    let mut memory = MemoryEventLog::new();
    let mem_results = drive_live(
        &[command],
        stamp,
        &surfaceless_world(),
        &StubClient,
        &NoSurfaceDrive,
        &UnattachedPeerSender,
        &mut memory,
    )
    .await
    .expect("the live driver dispatches against MemoryEventLog");
    let mem_lifecycle = memory.load_lifecycle().expect("load stratum-2");
    let mem_events = memory.load().expect("load stratum-1");
    assert!(
        matches!(
            mem_lifecycle.as_slice(),
            [LifecycleEvent::CommandDispatched { cmd: 3, .. }]
        ),
        "the MemoryEventLog stratum-2 stream holds the dispatch-intent for the cmd"
    );
    assert_eq!(
        mem_events.len(),
        1,
        "the MemoryEventLog stratum-1 stream holds exactly the result Event"
    );
    let mem_result = &mem_results[0];
    assert_eq!(mem_events[0], *mem_result, "the loaded result equals the fed-back result");
    assert_eq!(mem_result.origin, Origin::Agent, "result origin inherited from ctx (Inv 17)");
    assert_eq!(mem_result.edge, 9, "result edge inherited from ctx (Inv 17)");
}

// ---------------------------------------------------------------------------
// VC-2.2 — a log truncated mid-effect resumes; the entity settles, never stuck
// ---------------------------------------------------------------------------

/// **VC-2.2** A stratum-1 log truncated AFTER a `CommandDispatched { CallModel }` but
/// BEFORE its result (crash mid-`Thinking`): `resume` SYNTHESISES and LOGS a stratum-1
/// `InferenceCancelled { reason: Crash }` filling `cmd`/`entity`/`fingerprint` from the
/// dangling dispatch (envelope `origin`/`edge` inherited from its `ctx`, Inv 17), and
/// folding that synthesised Input through `tick` settles the entity `Thinking → Idle`
/// (totality — never a stuck sink, Inv 10). A fresh ACCOUNTED retry (new `cmd`, new
/// `key`) is autonomy/budget's job; resume NAMES it, it does not perform it.
#[test]
fn vc_2_2_truncated_log_resumes_and_settles_thinking_to_idle() {
    let model = offline_model();

    // Build the truncated tail in a MemoryEventLog under the log-before-apply
    // discipline, folding each Event so the rebuilt tail is a genuine `Thinking` state.
    let mut log = MemoryEventLog::new();
    let mut world = genesis(SEED, &model);

    let session = session_started(SEED);
    let user = user_message(1, "do the thing");

    log.append(&session).expect("append SessionStarted");
    let (next, _cmds) = tick(&world, &session);
    world = next;

    log.append(&user).expect("append UserMessage");
    let (next, commands) = tick(&world, &user);
    world = next;

    // The intake CallModel that the crash interrupted mid-flight. Its request is what
    // the surviving write-ahead dispatch-intent fingerprints (Inv 7).
    let (cmd, dispatch_fp) = commands
        .iter()
        .find_map(|c| match c {
            Command::CallModel {
                cmd,
                messages,
                tools,
                params,
                ..
            } => Some((
                *cmd,
                fingerprint_call(messages, tools, params).expect("fingerprint the intake request"),
            )),
            _ => None,
        })
        .expect("intake emits exactly one CallModel for the turn");

    // The entity is mid-`Thinking` on that cmd when the App crashes.
    assert!(
        matches!(
            world.entities.get(&0).expect("entity").activity,
            Activity::Thinking { cmd: c } if c == cmd
        ),
        "the turn dispatched a CallModel and the entity is Thinking on its cmd"
    );

    // The WRITE-AHEAD survived the crash: a stratum-2 `CommandDispatched { CallModel }`
    // whose result Input was NEVER logged — the dispatch is durable, the result is not
    // (intent before commitment, Inv 5). This is exactly what `drive_live` appends
    // first; here we record only the intent to model the crash between the two writes.
    let ctx = ActorCtx {
        entity: 0,
        origin: Origin::Agent,
        edge: 0,
    };
    let dispatch = LifecycleEvent::CommandDispatched {
        at: 2,
        cmd,
        kind: EffectKind::CallModel,
        ctx,
        key: IdempotencyKey {
            app_id: AppId::default(),
            tick: 2,
            effect_id: 0,
        },
        fingerprint: dispatch_fp.clone(),
    };
    log.append_lifecycle(&dispatch).expect("append the surviving dispatch-intent");

    // --- RESTART: load both strata; reconcile the dangling dispatch -----------
    let events = log.load().expect("load stratum-1 tail");
    let lifecycle = log.load_lifecycle().expect("load stratum-2 dispatch-intents");
    // The tail is genuinely truncated: it holds NO terminal result for the cmd.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelResponded { .. } | LogicalInput::InferenceCancelled { .. })),
        "the log is truncated mid-effect — no result for the dispatched cmd"
    );

    let reconciled = resume(&events, &lifecycle, &mut log).expect("resume reconciles the crash");

    // Exactly one reconciliation: the outstanding CallModel settles via a synthesised
    // InferenceCancelled{Crash} filling cmd/entity/fingerprint from the dangling
    // dispatch, the envelope inheriting origin/edge from its ctx (Inv 17).
    assert_eq!(reconciled.len(), 1, "the single dangling dispatch is reconciled");
    let settled = match &reconciled[0] {
        Reconciliation::Settled(event) => event,
        other => panic!("expected a Settled synthesised result, got {other:?}"),
    };
    assert_eq!(settled.origin, Origin::Agent, "envelope origin inherited from the dispatch ctx");
    assert_eq!(settled.edge, 0, "envelope edge inherited from the dispatch ctx");
    match &settled.input {
        LogicalInput::InferenceCancelled {
            cmd: ccmd,
            entity,
            fingerprint,
            partial,
            reason,
        } => {
            assert_eq!(*ccmd, cmd, "cmd filled from the dangling dispatch");
            assert_eq!(*entity, 0, "entity filled from the dispatch ctx");
            assert_eq!(*fingerprint, dispatch_fp, "fingerprint bound back to the dispatch");
            assert_eq!(*partial, None, "a crash mid-Thinking carries no partial");
            assert_eq!(*reason, CancelReason::Crash, "the cancellation reason is Crash");
        }
        other => panic!("expected InferenceCancelled, got {other:?}"),
    }

    // The crash is a REAL logged stratum-1 Input (not a silent rewind): the synthesised
    // cancellation is appended to the tail BEFORE being returned (log-before-apply,
    // Inv 4), so a subsequent replay reproduces the settled outcome deterministically.
    let after = log.load().expect("reload stratum-1 tail");
    assert_eq!(after.len(), events.len() + 1, "the synthesised cancellation was appended");
    assert!(
        matches!(
            &after.last().expect("tail").input,
            LogicalInput::InferenceCancelled { cmd: c, reason: CancelReason::Crash, .. } if *c == cmd
        ),
        "the synthesised InferenceCancelled{{Crash}} is durable in the tail"
    );

    // RESUME settles: folding the synthesised Input through the reducer takes the
    // replay-rebuilt `Thinking` tail to `Idle` — never a stuck sink (Inv 10). The
    // entity is now ready; the FRESH ACCOUNTED retry (a new cmd under a new key) is
    // decided by autonomy/budget — named here, NOT performed by resume.
    let (settled_world, continuation) = tick(&world, settled);
    assert!(
        continuation.is_empty(),
        "a crash cancellation emits no continuation here — the retry is autonomy/budget's job"
    );
    assert!(
        matches!(
            settled_world.entities.get(&0).expect("entity").activity,
            Activity::Idle
        ),
        "InferenceCancelled{{Crash}} settles Thinking → Idle (totality, Inv 10) — never stuck"
    );
}
