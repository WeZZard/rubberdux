//! **VC-2.2** — the compaction-resume LIVE loopback: a low `context_limit` triggers
//! a REAL compaction, a simulated crash mid-`Compacting` strands the session, and
//! `WorldDriver::open` RESUMES and COMPLETES the compaction with a REAL model call —
//! NO VM, no surface, no subprocess worker.
//!
//! The offline half (`tests/integration/agent/world_compaction_resume.rs`, VC-2.1)
//! proves — deterministically, non-vacuously — that a `Compacting` crash-tail
//! re-dispatches the `Compact` on `open()`, drives it to completion, and replays
//! byte-identically. This case proves the LIVE side the offline half cannot: that
//! the compaction the resume completes is a REAL model summarization call against a
//! real provider, and that the low-`context_limit` TRIGGER fires off a REAL turn's
//! real token usage. Per the mock-data policy (root `CLAUDE.md`) it uses the real
//! model and is gated on live-LLM credentials, skipping cleanly when absent.
//!
//! ## The construction and the proof
//! A single primary entity runs with `context_limit = 1` (so ANY real call's usage
//! overflows it) and the real env model. The loopback is two phases:
//!
//! **Phase 1 — the live low-context TRIGGER (real model call).** A user turn drives
//! the pure `tick` reducer to a `CallModel`, a SECOND user message parks in the
//! entity's Inbox (a continuation is pending), and the `CallModel` is dispatched
//! through the production [`drive_live`] against a REAL [`MessagesClient`]. Folding
//! the real `ModelResponded` settles the turn `Idle`, `BudgetSystem` folds the REAL
//! usage (over the limit), and `CompactionSystem` DEFERS the continuation:
//! `Idle → Compacting { cmd }` + a `Command::Compact`. The low `context_limit`
//! genuinely TRIGGERED a compaction off a real turn.
//!
//! **The simulated crash.** The emitted `Compact` is NOT dispatched. The
//! `Compacting` World is snapshotted and the in-flight `Compact` dispatch-intent is
//! recorded (the write-ahead that survived) with NO `Compacted` result — the
//! truncated mid-effect crash that STRANDS the session.
//!
//! **Phase 2 — the live RESUME (real model call).** `WorldDriver::open` reconstructs
//! the `Compacting` tail, RECONSTRUCTS the `Command::Compact` from the rebuilt
//! World's History, and RE-DISPATCHES it through `drive_live` against the REAL model
//! — a real summarization call — then drives the continuation it resumes (a second
//! real call) to quiescence.
//!
//! Three independent facts pin the live crash-resume:
//!
//! 1. **The trigger was real.** Folding the real `ModelResponded` left the primary
//!    `Compacting` with a `Compact` emitted — the low `context_limit` tripped on a
//!    real turn's real usage.
//! 2. **The resume completed the compaction with a real call.** The resumed log
//!    carries a real `Compacted` under the SAME `cmd` (NOT a `ModelFailed`), and the
//!    real model-call counter advanced during `open()`.
//! 3. **The session is quiescent.** Replaying the resumed log settles the primary
//!    `Idle` with NO entity left `Compacting` — the compaction is done.
//!
//! The dispatched result Events are persisted under
//! `tests/results/.../system/endurance_compaction_loopback/` for debugging (per
//! `tests/CLAUDE.md`).
//!
//! See `docs/agent/world/ecs-runtime.md` §1285-1318 (Context-window compaction;
//! compaction-resume re-dispatch) and the plan's Verification §2 VC-2.2.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value as Json;

use rubberdux::agent::world::budget::{Budget, Limits};
use rubberdux::agent::world::driver::WorldDriver;
use rubberdux::agent::world::edge::HUMAN_EDGE;
use rubberdux::agent::world::effects::{
    drive_live, Command, ModelCaller, ResultStamp, SurfaceDriver, UnattachedPeerSender,
};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{
    Event, Fingerprint, LogicalInput, ModelMeta, Origin,
};
use rubberdux::agent::world::lifecycle::{
    ActorCtx, AppId, EffectKind, IdempotencyKey, LifecycleEvent,
};
use rubberdux::agent::world::model_client::MessagesClient;
use rubberdux::agent::world::replay::restore;
use rubberdux::agent::world::snapshot::{capture, SnapshotStore};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources,
    Tick, World,
};
use rubberdux::error::Error;

use crate::live_gate::skip_without_live_llm;

// The hidden RNG seed crossing the recorded boundary (Inv 8).
const SEED: u64 = 53;

// The primary (lead) entity whose turn trips the compaction.
const PRIMARY: EntityId = 0;

// The context ceiling: `1` means ANY real call's token usage overflows it, so the
// real turn's real usage is guaranteed to TRIGGER a compaction (non-vacuous).
const CONTEXT_LIMIT: u32 = 1;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The genesis World: a single primary entity at `context_limit = 1` and the real
/// env model, with EMPTY surface tools so the live turns carry no tools and the
/// model answers with plain text (`EndTurn`), settling the turn `Idle` so the
/// compaction guard can fire.
fn genesis(model: &ModelConfig) -> World {
    let mut world = World::new(PRIMARY, Resources::new(SEED, model.clone()));
    world.entities.insert(
        PRIMARY,
        Components {
            identity: Identity::Primary,
            lineage: Lineage {
                parent: None,
                depth: 0,
            },
            history: History::default(),
            activity: Activity::Idle,
            gate: EntityGate::default(),
            // The low single-request context ceiling that the real turn's usage trips.
            budget: Budget {
                limits: Limits {
                    context_limit: CONTEXT_LIMIT,
                    ..Default::default()
                },
                ..Budget::default()
            },
            inbox: Inbox::default(),
            turns: 0,
            spawned: 0,
            model: None,
            autonomy: None,
        },
    );
    world
}

/// The real-env `ModelConfig` the live calls target. `model` is the alias
/// `MessagesClient::from_env` resolved from `RUBBERDUX_LLM_MODEL`; `max_tokens` from
/// `RUBBERDUX_LLM_MAX_TOKENS` (default 1024); effort `Medium`. Mirrors the sibling
/// endurance loopbacks.
fn env_model_config(model_alias: &str) -> ModelConfig {
    let max_tokens = std::env::var("RUBBERDUX_LLM_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1024);
    ModelConfig {
        model: model_alias.to_string(),
        max_tokens,
        effort: Effort::Medium,
    }
}

fn session_started() -> Event {
    Event {
        origin: Origin::System,
        edge: HUMAN_EDGE,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed: SEED,
            surface_tools: Vec::new(),
        },
    }
}

fn user_message(at: Tick, text: &str) -> Event {
    Event {
        origin: Origin::Human,
        edge: HUMAN_EDGE,
        at,
        wall: None,
        input: LogicalInput::UserMessage {
            to: PRIMARY,
            text: text.into(),
        },
    }
}

/// A `SurfaceDriver` that PANICS if reached: this loopback drives only model calls
/// (no surface write), so a reached `drive` is a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("the compaction loopback drives only model calls; the surface driver must not be reached");
    }
}

/// A `ModelCaller` that forwards to a REAL [`MessagesClient`] and COUNTS its calls,
/// so the test proves the resume advanced the REAL call count (the compaction +
/// continuation each made a real call). The guard is dropped before the await so the
/// future stays `Send`.
struct CountingCaller<'a> {
    inner: &'a MessagesClient,
    calls: Arc<AtomicUsize>,
}

impl ModelCaller for CountingCaller<'_> {
    async fn call(&self, request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.call(request_body).await
    }
}

/// An `EventLog` backed by two `Arc<Mutex<Vec<…>>>` so the test reads BOTH strata
/// (the crash-tail lives in stratum-2; the resume appends `Compacted` in stratum-1)
/// after `WorldDriver` owns the log. Cloning shares the same vectors.
#[derive(Clone)]
struct SharedTwoStratumLog {
    stratum1: Arc<Mutex<Vec<Event>>>,
    stratum2: Arc<Mutex<Vec<LifecycleEvent>>>,
}

impl SharedTwoStratumLog {
    fn new() -> Self {
        Self {
            stratum1: Arc::new(Mutex::new(Vec::new())),
            stratum2: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn events(&self) -> Vec<Event> {
        self.stratum1.lock().expect("lock stratum-1").clone()
    }
    fn lifecycle(&self) -> Vec<LifecycleEvent> {
        self.stratum2.lock().expect("lock stratum-2").clone()
    }
}

impl EventLog for SharedTwoStratumLog {
    fn append(&mut self, event: &Event) -> Result<(), Error> {
        self.stratum1.lock().expect("lock stratum-1").push(event.clone());
        Ok(())
    }
    fn load(&self) -> Result<Vec<Event>, Error> {
        Ok(self.events())
    }
    fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error> {
        self.stratum2.lock().expect("lock stratum-2").push(event.clone());
        Ok(())
    }
    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
        Ok(self.lifecycle())
    }
}

// ---------------------------------------------------------------------------
// VC-2.2 entry point (dispatched from `tests/system/main.rs`)
// ---------------------------------------------------------------------------

/// VC-2.2 — drive a real low-`context_limit` turn that triggers a compaction, crash
/// mid-`Compacting`, and prove `open()` resumes and completes the compaction with a
/// real model call, leaving the session quiescent.
pub async fn run() {
    if skip_without_live_llm("app::endurance_compaction_loopback (VC-2.2)") {
        return;
    }

    let client = MessagesClient::from_env()
        .expect("build a MessagesClient from RUBBERDUX_LLM_* for the live compaction turn");
    let model = env_model_config(client.model());
    eprintln!(
        "[VC-2.2] real env model={:?}; context_limit={} (any real usage overflows it → \
         a real turn TRIGGERS a compaction)",
        model.model, CONTEXT_LIMIT
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let caller = CountingCaller {
        inner: &client,
        calls: Arc::clone(&calls),
    };

    // Both strata are shared so the resume appends are observable after `open`.
    let mut log = SharedTwoStratumLog::new();
    let observe = log.clone();

    // ---------------------------------------------------------------------
    // Phase 1 — the live low-context TRIGGER (a real model call).
    // ---------------------------------------------------------------------
    let session = session_started();
    log.append(&session).expect("record the session header");
    let (mut world, _no_cmds) = tick(&genesis(&model), &session);

    // First user turn → a CallModel, entity Thinking.
    let turn1 = user_message(1, "Reply with one short fact about the number seven. One sentence.");
    log.append(&turn1).expect("record the first user message");
    let (next, commands) = tick(&world, &turn1);
    world = next;
    let call_model: Vec<Command> = commands
        .into_iter()
        .filter(|c| matches!(c, Command::CallModel { .. }))
        .collect();
    assert!(
        !call_model.is_empty(),
        "the first user turn must emit a CallModel"
    );

    // A SECOND user message parks in the Inbox (a continuation is pending) while the
    // entity is Thinking — the precondition CompactionSystem needs to DEFER.
    let turn2 = user_message(2, "Reply with one short fact about the number eight. One sentence.");
    log.append(&turn2).expect("record the second user message");
    let (next, _parked) = tick(&world, &turn2);
    world = next;
    assert!(
        world
            .entities
            .get(&PRIMARY)
            .map(|e| !e.inbox.pending.is_empty())
            .unwrap_or(false),
        "the second user message must park in the Inbox (a pending continuation)"
    );

    // Dispatch the first turn's CallModel LIVE — a REAL model call.
    let stamp = ResultStamp {
        edge: HUMAN_EDGE,
        app_edge: 1,
        at: 3,
        wall: None,
    };
    let results = drive_live(
        &call_model,
        stamp,
        &world,
        &caller,
        &NoSurfaceDrive,
        &UnattachedPeerSender,
        &mut log,
    )
    .await
    .expect("the live driver dispatches the first turn's CallModel against the real model");
    assert!(
        !results
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelFailed { .. })),
        "VC-2.2: the first real turn must succeed — no ModelFailed (results: {})",
        variant_dump(&results)
    );

    // Fold the real ModelResponded: TurnSystem settles Idle, BudgetSystem folds the
    // REAL usage (over context_limit), CompactionSystem DEFERS → Compacting + Compact.
    let mut compact_cmd: Option<CmdId> = None;
    for result in &results {
        let (next, cmds) = tick(&world, result);
        world = next;
        for cmd in cmds {
            if let Command::Compact { cmd: id, .. } = cmd {
                compact_cmd = Some(id);
            }
        }
    }
    let crash_cmd = compact_cmd.unwrap_or_else(|| {
        panic!(
            "VC-2.2: the low context_limit must TRIGGER a compaction after the real turn — \
             a Compact command was expected (primary activity: {:?})",
            world.entities.get(&PRIMARY).map(|e| &e.activity)
        )
    });
    assert!(
        matches!(
            world.entities.get(&PRIMARY).expect("primary").activity,
            Activity::Compacting { cmd } if cmd == crash_cmd
        ),
        "VC-2.2: the real turn's usage left the primary Compacting (the live trigger fired)"
    );
    let calls_after_trigger = calls.load(Ordering::SeqCst);
    eprintln!(
        "[VC-2.2] the real turn TRIGGERED a compaction: primary is Compacting {{ cmd={crash_cmd} }} \
         after {calls_after_trigger} real model call(s)"
    );

    // ---------------------------------------------------------------------
    // The simulated CRASH — snapshot the Compacting World and record the in-flight
    // Compact dispatch-intent (the write-ahead that survived) with NO Compacted.
    // ---------------------------------------------------------------------
    let dir = tempfile::tempdir().expect("create a tempdir for the snapshot store");
    let snapshots = dir.path().join("snapshots");
    SnapshotStore::new(&snapshots)
        .write(&capture(world.clone()))
        .expect("snapshot the Compacting World (the crash resume anchor)");
    log.append_lifecycle(&LifecycleEvent::CommandDispatched {
        at: world.clock + 1,
        cmd: crash_cmd,
        kind: EffectKind::Compact,
        ctx: ActorCtx {
            entity: PRIMARY,
            origin: Origin::Agent,
            edge: HUMAN_EDGE,
        },
        key: IdempotencyKey {
            app_id: AppId::default(),
            tick: world.clock + 1,
            effect_id: 0,
        },
        // The dispatch-intent fingerprint is a placeholder: the resume reconstructs
        // the Compact from the reconstructed World's History and reconciles the
        // outstanding cmd from its Compacting Activity, neither of which reads it.
        fingerprint: Fingerprint(format!("fp-compact-crash-{crash_cmd}")),
    })
    .expect("record the in-flight Compact dispatch-intent (the surviving write-ahead)");
    eprintln!(
        "[VC-2.2] CRASH injected: the Compact was dispatched (write-ahead) but never Compacted — \
         the session is stranded Compacting {{ cmd={crash_cmd} }}"
    );

    // ---------------------------------------------------------------------
    // Phase 2 — the live RESUME: open() re-dispatches the Compact (a REAL call) and
    // drives the compaction + continuation to quiescence.
    // ---------------------------------------------------------------------
    let driver = WorldDriver::open(
        SEED,
        model.clone(),
        caller,
        NoSurfaceDrive,
        log.clone(),
        SnapshotStore::new(&snapshots),
    )
    .await
    .expect("VC-2.2: open RESUMES and COMPLETES the interrupted compaction with a real model call");
    drop(driver);

    let calls_after_resume = calls.load(Ordering::SeqCst);
    let recorded = observe.events();
    let lifecycle = observe.lifecycle();

    // -- (1) The resume COMPLETED the compaction with a REAL model call ---------------
    assert!(
        recorded
            .iter()
            .any(|e| matches!(&e.input, LogicalInput::Compacted { cmd, .. } if *cmd == crash_cmd)),
        "VC-2.2: open must record a real Compacted under the same cmd (the compaction completed): {:?}",
        recorded.iter().map(variant_of).collect::<Vec<_>>()
    );
    assert!(
        !recorded
            .iter()
            .any(|e| matches!(&e.input, LogicalInput::ModelFailed { cmd, .. } if *cmd == crash_cmd)),
        "VC-2.2: the compaction call must SUCCEED against the real model — no ModelFailed for it"
    );
    assert!(
        calls_after_resume > calls_after_trigger,
        "VC-2.2: open advanced the REAL model-call count during the resume \
         ({calls_after_trigger} → {calls_after_resume}) — the compaction was a real call"
    );

    // -- (2) The session is QUIESCENT: replaying the resumed log settles Idle ---------
    let target: Tick = recorded.iter().map(|e| e.at).max().unwrap_or(0);
    let mut throwaway = MemoryEventLog::new();
    let resumed_world = restore(
        &SnapshotStore::new(&snapshots),
        &recorded,
        &lifecycle,
        // The snapshot encodes the Compacting World, so `restore` reconstructs from
        // it directly; this constructor (reseeded from the recorded header, Inv 8)
        // is the unused-for-the-snapshot-path genesis shape.
        |_seed| genesis(&model),
        target,
        &mut throwaway,
    )
    .expect("restore replays the resumed log from the crash snapshot anchor");
    assert!(
        !resumed_world
            .entities
            .values()
            .any(|c| matches!(c.activity, Activity::Compacting { .. })),
        "VC-2.2: the resumed session leaves no entity Compacting (the compaction is done)"
    );
    assert!(
        matches!(
            resumed_world.entities.get(&PRIMARY).expect("primary").activity,
            Activity::Idle
        ),
        "VC-2.2: the resumed continuation drove the primary back to Idle (quiescent)"
    );

    // -- Persist the collected transcript for debugging (tests/CLAUDE.md) -------------
    write_transcript(&recorded, crash_cmd, calls_after_resume);

    eprintln!(
        "[VC-2.2] PASS: a real turn under context_limit={} TRIGGERED a compaction (Compacting \
         {{ cmd={} }}); a crash mid-Compacting stranded the session; open() RESUMED and COMPLETED \
         the compaction with a real model call (Compacted recorded; {} → {} real calls) and the \
         session settled Idle — quiescent.",
        CONTEXT_LIMIT, crash_cmd, calls_after_trigger, calls_after_resume
    );
}

/// A compact variant dump of a result log, for self-explaining failure output.
fn variant_dump(events: &[Event]) -> String {
    events.iter().map(|e| variant_of(e)).collect::<Vec<_>>().join(", ")
}

/// A compact variant name for one event's `LogicalInput`.
fn variant_of(event: &Event) -> &'static str {
    match &event.input {
        LogicalInput::SessionStarted { .. } => "SessionStarted",
        LogicalInput::UserMessage { .. } => "UserMessage",
        LogicalInput::ModelResponded { .. } => "ModelResponded",
        LogicalInput::ModelFailed { .. } => "ModelFailed",
        LogicalInput::Compacted { .. } => "Compacted",
        _ => "<other>",
    }
}

/// The per-run results directory, under
/// `tests/results/<unix-millis>/system/endurance_compaction_loopback/` rooted at the
/// crate. A timestamped subdir keeps successive live runs from clobbering one another.
fn results_dir() -> std::path::PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("results")
        .join(format!("{millis}"))
        .join("system")
        .join("endurance_compaction_loopback")
}

/// Write the recorded log as raw JSONL plus a short markdown narration, so a failing
/// live run can be inspected (tests/CLAUDE.md). The narration records the crash cmd
/// and the real call count, the load-bearing facts of this case.
fn write_transcript(events: &[Event], crash_cmd: CmdId, real_calls: usize) {
    let dir = results_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut md = String::from("# Endurance compaction loopback — resumed result transcript\n\n");
    md.push_str(&format!(
        "- crash cmd (Compacting): {crash_cmd}\n- total real model calls: {real_calls}\n\n"
    ));
    for e in events {
        md.push_str(&format!(
            "- at={} edge={} origin={:?} — {}\n",
            e.at,
            e.edge,
            e.origin,
            variant_of(e)
        ));
    }
    let _ = std::fs::write(dir.join("narration.md"), md);
    if let Ok(serialized) = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
    {
        let _ = std::fs::write(dir.join("transcript.jsonl"), serialized.join("\n"));
    }
}
