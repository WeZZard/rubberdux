//! VC-2.1 — the OFFLINE compaction-resume sink, composed over the real
//! `src/agent/world/` stack (genesis, the pure `tick` reducer, `CompactionSystem`,
//! the `restore`/`fold_log` replay spine, and the live driver `WorldDriver::open`).
//! It touches no `src/agent/world/*` file.
//!
//! A session that crashed mid-compaction reconstructs the primary entity in
//! `Activity::Compacting { cmd }` — an in-flight `Command::Compact` whose
//! dispatch-intent survived (stratum-2 `CommandDispatched`) but whose `Compacted`
//! result was never logged (the truncated mid-effect crash). The CP-resume work in
//! `driver.rs` `open()` no longer fails loud on such a tail: it RECONSTRUCTS the
//! `Command::Compact` from the rebuilt World's tail Activity (the held `cmd` + the
//! unchanged History the deferral summarized) and RE-DISPATCHES it through
//! `drive_live` under the SAME `cmd`, driving the compaction — and the continuation
//! it resumes — to quiescence. This sink proves, OFFLINE and DETERMINISTICALLY:
//!
//! - **VC-2.1 (resume to completion).** `open()` over a `Compacting` crash-tail
//!   re-dispatches the `Compact`, makes EXACTLY two model calls (the compaction +
//!   the resumed continuation), leaves NO entity `Compacting` (the primary settles
//!   `Idle`), and logs the fresh `Compacted` under that same `cmd` — never a
//!   fail-loud, never a strand (Inv 10).
//! - **VC-2.1 (byte-identical replay).** The resumed log replays byte-identically:
//!   the production replay path (`restore`) and a direct `fold_log` from the
//!   compaction anchor reconstruct the SAME settled `Idle` World byte-for-byte
//!   (Inv 9), and two independent `open()` runs produce byte-identical recorded
//!   stratum-1 logs (deterministic content-addressed re-dispatch, Inv 7) — modulo
//!   the recorded wall hidden-input, which is observed data, not computed (Inv 2).
//! - **NON-VACUOUS.** A log WITHOUT a `Compacting` tail resumes UNCHANGED: `open()`
//!   makes ZERO model calls (an `ExplodingModelCaller` is structurally unreachable),
//!   and the resumed log's replay (`restore`) equals its `fold_log` byte-for-byte.
//!
//! Every proof here runs OFFLINE: a counting stub `ModelCaller`, no network, no
//! credentials. The LIVE half (a real low-`context_limit` trigger + a real
//! crash-resume model call) is `tests/system/app/endurance_compaction_loopback.rs`
//! (VC-2.2).
//!
//! See docs/agent/world/ecs-runtime.md §1285-1318 (Context-window compaction;
//! compaction-resume re-dispatch); Inv 7 (content-addressed replay), 9 (log is the
//! single source of truth), 10 (totality — never a stuck sink).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::driver::WorldDriver;
use rubberdux::agent::world::edge::HUMAN_EDGE;
use rubberdux::agent::world::effects::{Command, SurfaceDriver};
use rubberdux::provider::{ContentBlock, ModelApi, ModelInfo, ModelRequest, ModelResponse};
use std::future::Future;
use std::pin::Pin;

use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::lifecycle::{
    ActorCtx, AppId, EffectKind, IdempotencyKey, LifecycleEvent,
};
use rubberdux::agent::world::replay::{fold_log, restore};
use rubberdux::agent::world::snapshot::{capture, SnapshotStore};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources,
    Tick, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8) — reseeded identically
// for the live run and the replay so the rebuilt tail matches.
const SEED: u64 = 23;

// The primary (lead) entity, the only one this sink drives.
const PRIMARY: EntityId = 0;

// The cmd of the compaction call in flight when the App crashed — held in the
// reconstructed `Activity::Compacting { cmd }`.
const CMD: CmdId = 42;

// ---------------------------------------------------------------------------
// Genesis + fixtures — matching the driver's `genesis_world`
// ---------------------------------------------------------------------------

/// The world-default `ModelConfig`. Its `model` id rides in the compaction request
/// the re-dispatched `Compact` fingerprints, so the proof is deterministic; no real
/// call is made (the OFFLINE stub answers).
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-compaction-resume-sink".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// The fresh `World` a session starts from: tick 0, a single primary `Idle` entity
/// seeded from `seed`, with `model` as the world default. BYTE-IDENTICAL to the
/// driver's private `genesis_world`, so `WorldDriver::open`'s restore (which uses
/// `genesis_world`) and this harness's `fold_log` reconstruct from the same genesis
/// (Inv 6).
fn genesis(seed: u64, model: &ModelConfig) -> World {
    let mut world = World::new(PRIMARY, Resources::new(seed, model.clone()));
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

/// The `SessionStarted` header at tick 0 — empty surface tools (an offline session),
/// so the requests stay byte-identical to a pre-tools call.
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

/// The compaction RESUME ANCHOR: genesis + folded `SessionStarted`, the primary
/// entity forced into `Activity::Compacting { cmd: CMD }` over a NON-EMPTY History
/// (the older half is what the interrupted `Compact` was summarizing). Captured as
/// a snapshot so `restore` reconstructs this `Compacting` state directly (the
/// context conditions that produced it are exercised LIVE in the VC-2.2 loopback;
/// here the state itself is the fixture).
fn compacting_anchor(model: &ModelConfig) -> World {
    let (mut world, _no_commands) = tick(&genesis(SEED, model), &session_started());
    let e = world
        .entities
        .get_mut(&PRIMARY)
        .expect("the primary entity exists after genesis");
    e.history.0.push(Msg {
        role: Role::User,
        content: vec![Block::Text {
            text: "old question".into(),
        }],
    });
    e.history.0.push(Msg {
        role: Role::Assistant,
        content: vec![Block::Text {
            text: "old answer".into(),
        }],
    });
    e.activity = Activity::Compacting { cmd: CMD };
    world
}

/// The crash-tail streams: a stratum-1 log holding ONLY the `SessionStarted` header
/// (so the log is non-empty and `open` RESUMES) and a stratum-2 lifecycle holding
/// the in-flight `Compact` dispatch-intent (the write-ahead that survived) with NO
/// `Compacted` result — the truncated mid-effect crash. The dispatch-intent's
/// `fingerprint` is a placeholder: the resume reconstructs the `Compact` from the
/// reconstructed World's History (`rebuild_compact_command`) and reconciles the
/// outstanding cmd from its `Compacting` Activity, neither of which reads it.
fn crash_tail() -> (Vec<Event>, Vec<LifecycleEvent>) {
    let stratum1 = vec![session_started()];
    let stratum2 = vec![LifecycleEvent::CommandDispatched {
        at: 0,
        cmd: CMD,
        kind: EffectKind::Compact,
        ctx: ActorCtx {
            entity: PRIMARY,
            origin: Origin::Agent,
            edge: HUMAN_EDGE,
        },
        key: IdempotencyKey {
            app_id: AppId::default(),
            tick: 0,
            effect_id: 0,
        },
        fingerprint: Fingerprint(format!("fp-compact-{CMD}")),
    }];
    (stratum1, stratum2)
}

// ---------------------------------------------------------------------------
// Model-call stand-ins — the live driver's `ModelCaller` capability
// ---------------------------------------------------------------------------

/// A `ModelCaller` returning a fixed `EndTurn` response and COUNTING its calls, so a
/// resume proves EXACTLY two model calls (the re-dispatched `Compact` + the resumed
/// continuation) without a network. The compaction call reads the blocks as the
/// summary; the continuation reads them as its assistant reply — both `EndTurn`.
struct CountingStub {
    text: String,
    calls: Arc<AtomicUsize>,
}

impl ModelApi for CountingStub {
    fn turn<'a>(
        &'a self,
        _req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self.text.clone();
        Box::pin(async move {
            Ok(ModelResponse {
                blocks: vec![ContentBlock::Text { text }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                model_id: "claude-compaction-resume-sink".into(),
                reasoning: ReasoningPolicy::Drop,
                capabilities: serde_json::json!({}),
            })
        })
    }
    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn model(&self) -> &str {
        "claude-compaction-resume-sink"
    }
}

/// A `ModelCaller` that PANICS if invoked: a CLEAN resume (no `Compacting` tail)
/// reconstructs the World from the recorded log alone, so this client is
/// structurally unreachable — proving `open` makes ZERO model calls on it.
struct ExplodingModelCaller;

impl ModelApi for ExplodingModelCaller {
    fn turn<'a>(
        &'a self,
        _req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        panic!("a clean resume reconstructs from the log alone; it must never call the model");
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

/// A `SurfaceDriver` that PANICS if reached: every turn here drives only `CallModel`
/// / `Compact` (no `set_value` surface write), so a reached `drive` is a structural
/// error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("the compaction-resume sink drives only model calls; the surface driver is unused");
    }
}

// ---------------------------------------------------------------------------
// SharedTwoStratumLog — an `EventLog` whose BOTH strata the test reads after the
// driver owns the log
// ---------------------------------------------------------------------------

/// An `EventLog` backed by two `Arc<Mutex<Vec<…>>>` so the integration test can read
/// the events `WorldDriver` appended (BOTH strata — the crash-tail lives in
/// stratum-2 and the resume appends the `Compacted` in stratum-1) without touching
/// the driver's private `log` field. Cloning shares the same vectors; the test holds
/// one handle while a clone is moved into the driver.
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
// Helpers
// ---------------------------------------------------------------------------

/// The recorded artifacts and observations of one `open()` resume over a
/// `Compacting` crash-tail anchored under `dir`.
struct ResumeRun {
    /// The stratum-1 events the resumed session recorded (header + resume appends).
    events: Vec<Event>,
    /// The stratum-2 lifecycle the resumed session recorded.
    lifecycle: Vec<LifecycleEvent>,
    /// How many model calls the resume made (must be 2: compaction + continuation).
    calls: usize,
    /// The compaction anchor World (`Compacting { CMD }`) the snapshot held — the
    /// genesis a direct `fold_log` of the resumed tail reconstructs from.
    anchor: World,
}

/// Drive ONE `open()` resume over a freshly-anchored `Compacting` crash-tail rooted
/// at `dir`, returning the recorded artifacts. The anchor snapshot is written to
/// `dir/snapshots`, the crash-tail seeds a `SharedTwoStratumLog`, and `open`
/// re-dispatches the `Compact` and drives it (plus the continuation) to quiescence.
async fn open_resume(dir: &std::path::Path) -> ResumeRun {
    let model = offline_model();
    let anchor = compacting_anchor(&model);

    // Capture the `Compacting` anchor so `restore` reconstructs it directly.
    let snapshots = dir.join("snapshots");
    let writer = SnapshotStore::new(&snapshots);
    writer
        .write(&capture(anchor.clone()))
        .expect("write the compacting resume anchor");

    // Seed the crash-tail into a shared log; keep a handle to read the resume appends.
    let mut log = SharedTwoStratumLog::new();
    let (tail_events, tail_lifecycle) = crash_tail();
    for event in &tail_events {
        log.append(event).expect("seed the crash-tail header");
    }
    for record in &tail_lifecycle {
        log.append_lifecycle(record)
            .expect("seed the in-flight Compact dispatch-intent");
    }
    let observe = log.clone();

    let calls = Arc::new(AtomicUsize::new(0));
    let driver = WorldDriver::open(
        SEED,
        model.clone(),
        CountingStub {
            text: "summary of old turns".into(),
            calls: Arc::clone(&calls),
        },
        NoSurfaceDrive,
        log,
        SnapshotStore::new(&snapshots),
    )
    .await
    .expect("open resumes and completes the interrupted compaction");
    // The driver is consumed only for its append side effects on the shared log; it
    // exposes no public World accessor, so the resumed state is observed by replaying
    // the recorded log below (the log is the single source of truth, Inv 9).
    drop(driver);

    ResumeRun {
        events: observe.events(),
        lifecycle: observe.lifecycle(),
        calls: calls.load(Ordering::SeqCst),
        anchor,
    }
}

/// The reconstructed primary entity's `Activity`, by replaying the recorded log
/// through the production `restore` path anchored on the snapshot under `dir`.
fn reconstruct(dir: &std::path::Path, run: &ResumeRun) -> World {
    let model = offline_model();
    let target: Tick = run.events.iter().map(|e| e.at).max().unwrap_or(0);
    let mut throwaway = MemoryEventLog::new();
    restore(
        &SnapshotStore::new(dir.join("snapshots")),
        &run.events,
        &run.lifecycle,
        |seed| genesis(seed, &model),
        target,
        &mut throwaway,
    )
    .expect("restore replays the resumed log from the snapshot anchor")
}

/// Whether `world` holds any entity still `Compacting` — the strand the resume must
/// never leave behind.
fn any_compacting(world: &World) -> bool {
    world
        .entities
        .values()
        .any(|c| matches!(c.activity, Activity::Compacting { .. }))
}

/// A wall-normalized copy of the stratum-1 log: each event's recorded `wall`
/// (observed data, not computed — Inv 2) is cleared, so two independent live runs
/// compare byte-for-byte on everything the deterministic re-dispatch produces (cmds,
/// fingerprints, summary, tick ordering) without the real-clock hidden input.
fn wall_normalized(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.wall = None;
            e
        })
        .collect()
}

// ---------------------------------------------------------------------------
// VC-2.1 — a Compacting crash-tail re-dispatches on open and drives to completion
// ---------------------------------------------------------------------------

/// **VC-2.1 (resume to completion).** `open()` over a recorded session whose tail
/// reconstructs the primary in `Activity::Compacting { CMD }` re-dispatches the
/// `Compact`, drives it AND the continuation it resumes to quiescence, and:
///
/// - makes EXACTLY two model calls (the re-dispatched `Compact` + the resumed
///   continuation) — proving the session both COMPLETED the compaction and continued;
/// - logs a fresh `Compacted` under the SAME `CMD` (the compaction finished, durably);
/// - leaves NO entity `Compacting` and the primary settled `Idle` when the recorded
///   log is replayed — no fail-loud, no strand (Inv 10).
#[tokio::test]
async fn vc_2_1_compacting_crash_tail_redispatches_on_open_and_completes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = open_resume(dir.path()).await;

    // Two model calls: the re-dispatched Compact + the resumed continuation.
    assert_eq!(
        run.calls, 2,
        "open re-dispatched the Compact and drove the continuation — two model calls"
    );

    // The fresh Compacted was appended to the log under the SAME cmd (compaction done).
    assert!(
        run.events
            .iter()
            .any(|e| matches!(&e.input, LogicalInput::Compacted { cmd, .. } if *cmd == CMD)),
        "the re-dispatch recorded a Compacted result under the same cmd: {:?}",
        run.events.iter().map(input_variant).collect::<Vec<_>>()
    );

    // Replaying the resumed log settles the primary to Idle with NO entity Compacting
    // — the compaction was driven to completion, not stranded.
    let world = reconstruct(dir.path(), &run);
    assert!(
        !any_compacting(&world),
        "replaying the resumed log leaves no entity Compacting (no strand, Inv 10)"
    );
    assert!(
        matches!(
            world.entities.get(&PRIMARY).expect("primary entity").activity,
            Activity::Idle
        ),
        "the resumed continuation drove the primary back to Idle"
    );
}

// ---------------------------------------------------------------------------
// VC-2.1 — the resumed log replays byte-identically
// ---------------------------------------------------------------------------

/// **VC-2.1 (byte-identical replay).** The resumed log replays byte-identically:
///
/// 1. The production replay path (`restore`, snapshot → tail fold) and a DIRECT
///    `fold_log` from the compaction anchor reconstruct the SAME settled World
///    byte-for-byte (Inv 9) — both `Idle`, neither `Compacting`.
/// 2. Two INDEPENDENT `open()` runs over the identical crash-tail produce
///    byte-identical recorded stratum-1 logs once the recorded wall hidden input is
///    normalized away (deterministic, content-addressed re-dispatch — Inv 7).
#[tokio::test]
async fn vc_2_1_resumed_compacting_log_replays_byte_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = open_resume(dir.path()).await;

    // (1) restore (snapshot → tail fold) == fold_log (anchor → tail), byte-for-byte.
    // Both consume the SAME recorded tail (same walls), so this is wall-stable.
    let replayed = reconstruct(dir.path(), &run);
    let tail: Vec<Event> = run
        .events
        .iter()
        .filter(|e| e.at > run.anchor.clock)
        .cloned()
        .collect();
    let folded = fold_log(run.anchor.clone(), &tail);
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize the restored World"),
        serde_json::to_vec(&folded).expect("serialize the folded World"),
        "the resumed log's production replay (restore) equals its direct fold (Inv 9)"
    );
    assert!(
        !any_compacting(&folded)
            && matches!(
                folded.entities.get(&PRIMARY).expect("primary").activity,
                Activity::Idle
            ),
        "the replayed World is the settled Idle state (the compaction completed)"
    );

    // (2) A second independent run yields a byte-identical (wall-normalized) log.
    let dir2 = tempfile::tempdir().expect("tempdir");
    let run2 = open_resume(dir2.path()).await;
    assert_eq!(
        serde_json::to_vec(&wall_normalized(&run.events)).expect("serialize run 1 log"),
        serde_json::to_vec(&wall_normalized(&run2.events)).expect("serialize run 2 log"),
        "two independent resumes record byte-identical stratum-1 logs (deterministic \
         re-dispatch, Inv 7) — modulo the recorded wall hidden input (Inv 2)"
    );
    assert_eq!(run2.calls, 2, "the second resume likewise makes exactly two model calls");
}

// ---------------------------------------------------------------------------
// NON-VACUOUS — a log WITHOUT a Compacting tail resumes unchanged
// ---------------------------------------------------------------------------

/// **NON-VACUOUS.** A recorded session WITHOUT a `Compacting` tail (a plain
/// `Idle → Thinking → Idle` turn) resumes UNCHANGED: `open()` makes ZERO model calls
/// (an `ExplodingModelCaller` is structurally unreachable, in contrast to the two
/// calls the `Compacting` tail forces), and the resumed log's replay (`restore`,
/// here an empty-store genesis fallback) equals its `fold_log` from genesis
/// byte-for-byte (Inv 9), with no entity left `Compacting`. This pins that the
/// re-dispatch fires ONLY on a genuine `Compacting` tail.
#[tokio::test]
async fn non_vacuous_clean_log_without_compacting_tail_resumes_unchanged() {
    let model = offline_model();

    // Author a clean one-turn log: SessionStarted, UserMessage (→ Thinking{cmd}),
    // ModelResponded·EndTurn (→ Idle). Discover the minted cmd by folding so the
    // ModelResponded correlates to the turn's CallModel.
    let session = session_started();
    let user = Event {
        origin: Origin::Human,
        edge: HUMAN_EDGE,
        at: 1,
        wall: None,
        input: LogicalInput::UserMessage {
            to: PRIMARY,
            text: "ping".into(),
        },
    };
    let (after_session, _no_cmds) = tick(&genesis(SEED, &model), &session);
    let (_after_user, commands) = tick(&after_session, &user);
    let cmd = commands
        .iter()
        .find_map(|c| match c {
            Command::CallModel { cmd, .. } => Some(*cmd),
            _ => None,
        })
        .expect("the clean turn emits a CallModel");
    let responded = Event {
        origin: Origin::Agent,
        edge: HUMAN_EDGE,
        at: 2,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: PRIMARY,
            fingerprint: Fingerprint(String::new()),
            blocks: vec![Block::Text { text: "pong".into() }],
            meta: ModelMeta {
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                model_id: "claude-compaction-resume-sink".into(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    };
    let events = vec![session, user, responded];

    // Pre-condition: the clean log folds to a settled Idle World, no Compacting.
    let folded = fold_log(genesis(SEED, &model), &events);
    assert!(
        !any_compacting(&folded)
            && matches!(
                folded.entities.get(&PRIMARY).expect("primary").activity,
                Activity::Idle
            ),
        "the clean turn folds to a settled Idle World (no Compacting tail)"
    );

    // open() with an EMPTY store + an ExplodingModelCaller: a clean resume needs ZERO
    // model calls, so reaching Ok proves the re-dispatch loop never fired.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut log = MemoryEventLog::new();
    for event in &events {
        log.append(event).expect("seed the clean log");
    }
    let driver = WorldDriver::open(
        SEED,
        model.clone(),
        ExplodingModelCaller,
        NoSurfaceDrive,
        log,
        SnapshotStore::new(dir.path().join("snapshots")),
    )
    .await
    .expect("open cleanly resumes a log without a Compacting tail");
    drop(driver);

    // The resumed log's replay (restore, empty-store genesis fallback) equals its
    // fold from genesis byte-for-byte (Inv 9) — the clean resume is unchanged.
    let mut throwaway = MemoryEventLog::new();
    let replayed = restore(
        &SnapshotStore::new(dir.path().join("snapshots")),
        &events,
        &[],
        |seed| genesis(seed, &model),
        2,
        &mut throwaway,
    )
    .expect("restore replays the clean log via the genesis fallback");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize the restored World"),
        serde_json::to_vec(&folded).expect("serialize the folded World"),
        "a clean resume's replay equals its fold from genesis (Inv 9)"
    );
    assert!(
        !any_compacting(&replayed),
        "a clean resume leaves no entity Compacting"
    );
}

/// A compact variant name for a `LogicalInput`, for self-explaining failure output.
fn input_variant(event: &Event) -> &'static str {
    match &event.input {
        LogicalInput::SessionStarted { .. } => "SessionStarted",
        LogicalInput::UserMessage { .. } => "UserMessage",
        LogicalInput::ModelResponded { .. } => "ModelResponded",
        LogicalInput::ModelFailed { .. } => "ModelFailed",
        LogicalInput::Compacted { .. } => "Compacted",
        _ => "<other>",
    }
}
