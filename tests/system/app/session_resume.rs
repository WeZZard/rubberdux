//! **VC-4.1** — the runtime resumes an existing session on restart (US-7).
//!
//! This is the System-scope proof that the worker's resume path (`open_world_driver`
//! → `WorldDriver::open` over the session resolved from the `latest` link) makes a
//! RESTARTED App continue its recorded World rather than start a fresh genesis. The
//! lib-scope half (`src/agent/world/driver.rs`'s
//! `open_reconstructs_prior_world_and_continues_at_the_right_tick` /
//! `resumed_driver_emits_only_new_text_and_continues_ticks`) proves the
//! reconstruction byte-identically with stubs; this case proves the SAME flow
//! end-to-end against a REAL recorded session and a REAL model. Per the mock-data
//! policy (root `CLAUDE.md`) it uses the real `MessagesClient` and is gated on
//! live-LLM credentials, skipping cleanly when absent.
//!
//! ## Why drive `WorldDriver::open` directly
//! The worker's resume seam (`open_world_driver` / `latest_session` /
//! `seed_from_session` in `src/app/runtime/worker.rs`) is private, so it is not
//! reachable from a test crate. Per the plan, the faithful TESTABLE seam is to drive
//! `WorldDriver::open` over the SAME session dir the worker would resolve — the
//! EXACT mechanism the worker uses: it resolves the latest session via the `latest`
//! symlink (mirrored here by [`resolve_latest`]), roots a `FilesystemEventLog` at
//! `<session_dir>/world-events.jsonl` and a `SnapshotStore` at
//! `<session_dir>/snapshots/`, derives the per-session seed (mirrored by
//! [`seed_from_session`]), and calls `WorldDriver::open`. This case reproduces that
//! resolution and asserts the worker's session resolution separately (the restart
//! reopens the SAME `latest`-linked session, not a new one).
//!
//! ## The session and the proof
//! 1. **Record (1 real call).** Root a `SessionManager` at an ISOLATED temp
//!    app-home (so the developer's real `$RUBBERDUX_HOME` / `~/.rubberdux` is never
//!    touched), `create_session` (which stamps the `latest` link), then
//!    `WorldDriver::open` over its EMPTY log (delegating to `bootstrap`) and drive
//!    ONE live turn (`q1 → a1`). The recorded `world-events.jsonl` is now non-empty:
//!    `[SessionStarted, UserMessage(q1), ModelResponded(a1)]`, frontier tick `F`.
//!    A snapshot at a mid tick is captured into the session's `snapshots/` store to
//!    stand in for the bounded snapshots a long-running session accumulates (the
//!    periodic writer's 64-tick interval is impractical for a 2-call live test), so
//!    the resume path is also proven to reconstruct from snapshot + tail.
//! 2. **Restart, structural (0 calls).** Reopen the SAME latest session via
//!    `WorldDriver::open` with an `ExplodingModelCaller`: a clean resume
//!    reconstructs from the log + snapshot ALONE, so the exploding client is
//!    structurally unreachable — proving `open` never re-runs the recorded turn nor
//!    re-genesises. The recorded log is unchanged by a clean resume.
//! 3. **Restart, live (1 real call).** Reopen the SAME session with the REAL
//!    client and drive ONE NEW turn (`q2 → a2`, a DIFFERENT question). Assert: the
//!    reconstructed base equals the pre-restart World (the `at ≤ F` prefix replays
//!    byte-identically to the pre-restart World — equal digest AND clock, NOT a
//!    fresh genesis); the resumed turn is stamped at `F + 1` (strictly past the
//!    recorded frontier, monotonic continuation, Inv 8/12); the old assistant text
//!    `a1` is NOT re-emitted (only `a2`); the SAME `world-events.jsonl` dir is
//!    reused (no new session minted); and the snapshot still lives under the
//!    session dir.
//!
//! See `docs/agent/world/ecs-runtime.md` (Restore snapshots; Inv 6 replay
//! determinism, Inv 8 recorded seed) and the plan's Verification §4 VC-4.1 / §US-7.

use std::path::PathBuf;

use serde_json::Value as Json;

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::driver::WorldDriver;
use rubberdux::agent::world::effects::{Command, ModelCaller, SurfaceDriver};
use rubberdux::agent::world::event_log::{EventLog, FilesystemEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{Event, LogicalInput, ModelMeta, Origin};
use rubberdux::agent::world::model_client::MessagesClient;
use rubberdux::agent::world::replay::{self, is_model_call_result, replay_world};
use rubberdux::agent::world::snapshot::{snapshot_at, SnapshotStore};
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, Tick, World,
};
use rubberdux::error::Error;
use rubberdux::session::{SessionId, SessionManager};

use crate::live_gate::skip_without_live_llm;

/// A fixed App id for the resumed session: the worker derives the per-session RNG
/// seed from `"{app_id}:{session_id}"`, so record and restart must agree on it.
const APP_ID: &str = "session-resume-vc41";

// ---------------------------------------------------------------------------
// Genesis — reproduced IDENTICALLY to the worker's `driver::genesis_world` so a
// reconstruction from the recorded log byte-equals what `WorldDriver::open` builds.
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from, mirroring `WorldDriver::bootstrap`'s
/// genesis (the private `driver::genesis_world` + `primary_idle_components`): tick
/// 0, a single primary `Idle` root entity, `Resources` seeded from `seed` with the
/// supplied `model`. A reconstruction reseeds from the recorded `SessionStarted`
/// header, so this constructor is the same the live run used (genesis-parity).
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

/// The genesis `ModelConfig` shared by the recording and every restart. `model` is
/// the alias resolved from `RUBBERDUX_LLM_MODEL` (so a REAL call targets a valid
/// model); `max_tokens` reads `RUBBERDUX_LLM_MAX_TOKENS` (default 1024 — a one-word
/// answer stops well short). Because the recording and the restart reconstruct from
/// the SAME genesis `ModelConfig`, the reconstruction is byte-identical (Inv 6).
fn model_config(model_alias: &str) -> ModelConfig {
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

/// Derive the per-session RNG seed exactly as the worker's `seed_from_session`
/// does — a pure FNV-1a over `"{app_id}:{session_id}"`. A reconstruction reseeds
/// from the recorded header regardless, but reproducing this keeps the recorded
/// `SessionStarted.seed` identical to what the worker would write.
fn seed_from_session(app_id: &str, session_id: &SessionId) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in format!("{app_id}:{}", session_id.to_string()).bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Resolve the latest session from the `latest` symlink, exactly as the worker's
/// private `latest_session` does: `Some((id, dir))` when the link resolves to a
/// session directory whose name parses as a [`SessionId`], else `None`.
fn resolve_latest(manager: &SessionManager) -> Option<(SessionId, PathBuf)> {
    let target = std::fs::read_link(manager.latest_link()).ok()?;
    let name = target.file_name()?.to_str()?;
    let session_id = SessionId::from_string(name)?;
    let session_dir = manager.session_dir(&session_id);
    session_dir.is_dir().then_some((session_id, session_dir))
}

/// Reconstruct the recorded World from its event log under the FAITHFUL replay
/// spine (`replay::replay_world`) — the same reconstruction `WorldDriver::open`
/// performs (genesis reseeded from the header + cursor-driven fold with zero model
/// calls). The result is what `open` continues from, so its canonical bytes are the
/// pre-restart World's digest and its `clock` the frontier.
fn reconstruct(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    let genesis = replay::genesis_from_log(events, |seed| genesis(seed, model))?;
    replay_world(genesis, events, is_model_call_result)
}

/// The canonical serialization whose SHA-256 is the snapshot/replay digest (Inv 7).
/// Byte-equality of two canonical encodings IS digest-equality, so the test
/// compares these directly (as the offline counterfactual sink does).
fn canonical(world: &World) -> Vec<u8> {
    serde_json::to_vec(world).expect("World serializes (no floats, no cycles)")
}

// ---------------------------------------------------------------------------
// IO capabilities for the in-process driver
// ---------------------------------------------------------------------------

/// A `ModelCaller` that PANICS if invoked: a clean resume reconstructs the World
/// from the recorded log + snapshot ALONE (zero model calls), so on the structural
/// restart this client is unreachable — proving `open` never re-runs the recorded
/// turn nor re-genesises.
struct ExplodingModelCaller;

impl ModelCaller for ExplodingModelCaller {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        panic!("a clean resume reconstructs from the log + snapshot alone; it must never call the model");
    }
}

/// A `SurfaceDriver` that ACCEPTS every drive as a no-op, mirroring the production
/// `WorkerSurfaceDriver` (which only forwards a frame and never errors). The
/// recorded session is a plain CallModel turn (the "one word, no tools" prompt), so
/// no surface drive is expected; accepting rather than panicking keeps the test
/// robust against an incidental tool request.
struct AcceptingSurfaceDriver;

impl SurfaceDriver for AcceptingSurfaceDriver {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}

/// The concrete recording/resuming driver: the real `MessagesClient`, the accepting
/// surface sink, and a `FilesystemEventLog` rooted under the session dir.
type LiveDriver = WorldDriver<MessagesClient, AcceptingSurfaceDriver, FilesystemEventLog>;

/// The structural restart driver: an `ExplodingModelCaller` proves a clean resume
/// makes zero model calls.
type DryDriver = WorldDriver<ExplodingModelCaller, AcceptingSurfaceDriver, FilesystemEventLog>;

/// The first non-empty assistant text across a turn's notifications (the turn's
/// reply), or `None` when the turn produced no assistant text.
fn assistant_reply(notifications: &[rubberdux::agent::runtime::port::EntryNotification]) -> Option<String> {
    notifications
        .iter()
        .map(|n| n.entry.message.content_text().to_string())
        .find(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// VC-4.1 — record a real session, restart, prove resume
// ---------------------------------------------------------------------------

/// VC-4.1 entry point (dispatched from `tests/system/main.rs`). Records a short
/// real session in an isolated temp app-home, restarts the runtime over the SAME
/// latest session, and proves the prior World is reconstructed (digest/clock equal,
/// not genesis), the resumed driver continues at the correct next tick, it does not
/// re-emit the old assistant text, and snapshots land under the session dir.
pub async fn run() {
    if skip_without_live_llm("app::session_resume (VC-4.1)") {
        return;
    }

    // ISOLATION: root the SessionManager at a temp app-home rather than the
    // developer's real `$RUBBERDUX_HOME` (the env var `SessionManager::resolve_home`
    // honors) / `~/.rubberdux`, so the test never touches real sessions. The
    // TempDir cleans the home on drop. The layout mirrors the worker's
    // `session_manager_at`.
    let app_home = tempfile::tempdir().expect("create an isolated temp app-home");
    let manager = SessionManager {
        home_dir: app_home.path().to_path_buf(),
        sessions_dir: app_home.path().join("sessions"),
        latest_link: app_home.path().join("latest"),
    };

    let client = MessagesClient::from_env()
        .expect("build a MessagesClient from RUBBERDUX_LLM_* for the live recording");
    let model = model_config(client.model());
    let model_alias = client.model().to_owned();
    eprintln!(
        "[VC-4.1] resume ModelConfig: model={:?} max_tokens={} effort={:?}",
        model.model, model.max_tokens, model.effort
    );

    // -- (1) Record a short real session (1 real model call) ------------------
    // Create the session (stamps the `latest` link), open the driver over its
    // EMPTY log (→ bootstrap), and drive ONE live turn so the log is non-empty and
    // carries a recorded assistant reply.
    let (session_id, session_dir) = manager
        .create_session(model_alias)
        .expect("create the recording session");
    let events_path = session_dir.join("world-events.jsonl");
    let snapshots_dir = session_dir.join("snapshots");
    let seed = seed_from_session(APP_ID, &session_id);

    let recorded_reply = {
        let event_log = FilesystemEventLog::new(&events_path);
        let store = SnapshotStore::new(&snapshots_dir);
        let mut driver: LiveDriver = WorldDriver::open(
            seed,
            model.clone(),
            client,
            AcceptingSurfaceDriver,
            event_log,
            store,
        )
        .await
        .expect("open a fresh recording session over the empty log");

        let notifications = driver
            .submit(LogicalInput::UserMessage {
                to: 0,
                text: "Reply with exactly one word and call no tools: the capital of France.".into(),
            })
            .await
            .expect("the recorded turn drives to quiescence (real model call)");

        assistant_reply(&notifications)
            .expect("the recorded turn produced a non-empty assistant reply")
        // `driver` (and its event log / client) drop here — the "shutdown" before
        // the restart.
    };

    // Load the recorded log: it must be non-empty and hold a real `ModelResponded`.
    let recording_log = FilesystemEventLog::new(&events_path);
    let pre_restart_events = recording_log.load().expect("load the recorded world-events.jsonl");
    assert!(
        !pre_restart_events.is_empty(),
        "VC-4.1: the recorded session log must be non-empty"
    );
    assert!(
        pre_restart_events
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelResponded { .. })),
        "VC-4.1: the recording must hold a real ModelResponded reply (the live call must \
         have succeeded — check RUBBERDUX_LLM_* credentials)"
    );

    // The pre-restart World and frontier the restart must reconstruct + continue past.
    let pre_restart_world =
        reconstruct(&pre_restart_events, &model).expect("reconstruct the pre-restart World");
    let pre_restart_digest = canonical(&pre_restart_world);
    let frontier: Tick = pre_restart_events
        .iter()
        .map(|e| e.at)
        .max()
        .expect("the recorded log carries a tick");
    assert_eq!(
        pre_restart_world.clock, frontier,
        "VC-4.1: the reconstructed clock equals the recorded frontier"
    );
    // NOT a fresh genesis: the prior World has advanced past tick 0 and diverges
    // from a fresh genesis World.
    assert!(frontier > 0, "VC-4.1: the recorded frontier advanced past genesis");
    assert_ne!(
        pre_restart_digest,
        canonical(&genesis(seed, &model)),
        "VC-4.1: the pre-restart World is not a fresh genesis"
    );

    // Stand-in bounded snapshot: capture the recorded World at a mid tick into the
    // session's `snapshots/` store, so the resume path also reconstructs from
    // snapshot + tail and snapshots demonstrably land under the session dir. (A
    // long session's periodic writer would create these; the 64-tick interval is
    // impractical for a 2-call live test.)
    let snap_tick: Tick = pre_restart_events
        .iter()
        .map(|e| e.at)
        .filter(|&t| t < frontier)
        .max()
        .unwrap_or(0);
    {
        let store = SnapshotStore::new(&snapshots_dir);
        let snapshot = snapshot_at(&pre_restart_events, |s| genesis(s, &model), snap_tick)
            .expect("capture a mid-tick snapshot of the recorded World");
        store.write(&snapshot).expect("persist the snapshot under the session dir");
    }
    assert!(
        snapshots_dir.join(format!("{snap_tick}.snapshot.json")).is_file(),
        "VC-4.1: a snapshot file lands under <session_dir>/snapshots/"
    );

    // The worker resolves the session to resume from the `latest` link. Assert that
    // resolution lands on the SAME session we recorded (the restart reopens it, not
    // a new one).
    let (resolved_id, resolved_dir) =
        resolve_latest(&manager).expect("the latest link resolves to the recorded session");
    assert_eq!(
        resolved_id.to_string(),
        session_id.to_string(),
        "VC-4.1: the worker's latest-link resolution reopens the recorded session"
    );
    assert_eq!(
        resolved_dir, session_dir,
        "VC-4.1: the resolved session dir is the recorded one (same world-events.jsonl dir)"
    );

    // -- (2) Restart, structural: a clean resume makes ZERO model calls --------
    // Reopen the SAME latest session with an exploding client: if `open` succeeds,
    // it reconstructed from the log + snapshot alone (never re-ran the recorded turn
    // nor re-genesised). A clean resume appends nothing, so the log is unchanged.
    {
        let event_log = FilesystemEventLog::new(&resolved_dir.join("world-events.jsonl"));
        let store = SnapshotStore::new(resolved_dir.join("snapshots"));
        let _dry: DryDriver = WorldDriver::open(
            seed,
            model.clone(),
            ExplodingModelCaller,
            AcceptingSurfaceDriver,
            event_log,
            store,
        )
        .await
        .expect("a clean resume reconstructs from the log + snapshot with zero model calls");
    }
    let after_dry = FilesystemEventLog::new(&events_path)
        .load()
        .expect("reload the log after the structural restart");
    assert_eq!(
        after_dry, pre_restart_events,
        "VC-4.1: a clean resume does not mutate the recorded log"
    );

    // -- (3) Restart, live: continue the session with a NEW turn ---------------
    // Reopen the SAME session with the REAL client and drive a DIFFERENT question
    // so its reply is distinguishable from the recorded one.
    let restart_client = MessagesClient::from_env()
        .expect("rebuild a MessagesClient for the live restart turn");
    let restart_reply = {
        let event_log = FilesystemEventLog::new(&resolved_dir.join("world-events.jsonl"));
        let store = SnapshotStore::new(resolved_dir.join("snapshots"));
        let mut driver: LiveDriver = WorldDriver::open(
            seed,
            model.clone(),
            restart_client,
            AcceptingSurfaceDriver,
            event_log,
            store,
        )
        .await
        .expect("the restart reopens the recorded session via WorldDriver::open");

        let notifications = driver
            .submit(LogicalInput::UserMessage {
                to: 0,
                text: "Reply with exactly one word and call no tools: the capital of Japan.".into(),
            })
            .await
            .expect("the resumed turn drives to quiescence (real model call)");

        // The resumed driver does NOT re-emit the old assistant text — the history
        // watermark suppresses already-delivered messages (Inv 8).
        assert!(
            notifications
                .iter()
                .all(|n| n.entry.message.content_text() != recorded_reply),
            "VC-4.1: the resumed turn must not re-emit the recorded assistant text {recorded_reply:?}"
        );
        assistant_reply(&notifications)
            .expect("the resumed turn produced a non-empty assistant reply")
    };
    assert_ne!(
        restart_reply.to_lowercase(),
        recorded_reply.to_lowercase(),
        "VC-4.1: the new turn's reply differs from the recorded one (different question)"
    );

    // -- Reconstruction + continuation, observed via the post-restart log ------
    let post_restart_events = FilesystemEventLog::new(&events_path)
        .load()
        .expect("reload the log after the live restart turn");

    // The SAME world-events.jsonl dir was reused: the recorded prefix is unchanged
    // and the new turn was appended after it (no new session minted).
    let prefix_at_or_before_frontier: Vec<Event> = post_restart_events
        .iter()
        .filter(|e| e.at <= frontier)
        .cloned()
        .collect();
    assert_eq!(
        prefix_at_or_before_frontier, pre_restart_events,
        "VC-4.1: the resumed run reuses the SAME world-events.jsonl — the recorded prefix is intact"
    );
    assert!(
        post_restart_events.len() > pre_restart_events.len(),
        "VC-4.1: the resumed turn appended its events to the existing log"
    );

    // The reconstruction base equals the pre-restart World byte-identically (equal
    // digest AND clock), NOT a fresh genesis: the `at ≤ F` prefix of the resumed log
    // replays to exactly the pre-restart World.
    let reconstructed_base =
        reconstruct(&prefix_at_or_before_frontier, &model).expect("replay the reconstruction base");
    assert_eq!(
        canonical(&reconstructed_base),
        pre_restart_digest,
        "VC-4.1: the resumed run's reconstruction base is digest-equal to the pre-restart World"
    );
    assert_eq!(
        reconstructed_base.clock, frontier,
        "VC-4.1: the reconstruction base's clock equals the pre-restart frontier"
    );

    // The resumed turn continues at the correct next tick — strictly past the
    // recorded frontier (monotonic, Inv 8/12), never colliding with recorded ticks.
    let resumed_user_message = post_restart_events
        .iter()
        .find(|e| e.at > frontier && matches!(e.input, LogicalInput::UserMessage { .. }))
        .expect("the resumed UserMessage is stamped past the recorded frontier");
    assert_eq!(
        resumed_user_message.at,
        frontier + 1,
        "VC-4.1: the resumed turn continues at the reconstructed clock + 1 ({}), not a re-genesis",
        frontier + 1
    );
    assert_eq!(
        resumed_user_message.origin,
        Origin::Human,
        "VC-4.1: the resumed UserMessage is a human-origin free variable"
    );

    // The full post-restart log replays faithfully (zero model calls) to a World
    // that CONTINUED the prior one: its clock advanced past the frontier and its
    // canonical bytes differ from the pre-restart World (the new turn folded in).
    let post_restart_world =
        reconstruct(&post_restart_events, &model).expect("replay the full post-restart log");
    assert!(
        post_restart_world.clock > frontier,
        "VC-4.1: the resumed World's clock advanced past the recorded frontier"
    );
    assert_ne!(
        canonical(&post_restart_world),
        pre_restart_digest,
        "VC-4.1: the resumed World extended the prior one (it is not still the pre-restart World)"
    );

    // Snapshots still live under the session dir after the restart.
    assert!(
        snapshots_dir.join(format!("{snap_tick}.snapshot.json")).is_file(),
        "VC-4.1: the snapshot still lives under <session_dir>/snapshots/ after the restart"
    );

    // Exactly ONE session dir exists — the restart resumed, never minted a new one.
    let session_count = std::fs::read_dir(manager.sessions_dir())
        .expect("read the sessions dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .count();
    assert_eq!(
        session_count, 1,
        "VC-4.1: the restart resumed the existing session — no new session dir was minted"
    );

    eprintln!(
        "[VC-4.1] PASS: recorded session {} (frontier tick {frontier}, reply {recorded_reply:?}); \
         restart reconstructed the prior World (digest+clock equal, zero model calls), continued \
         at tick {} with reply {restart_reply:?} without re-emitting the old text, reused the same \
         world-events.jsonl dir, and the snapshot lives under the session dir.",
        session_id.to_string(),
        frontier + 1
    );

    // Explicit cleanup of the isolated app-home (TempDir also removes it on drop).
    drop(app_home);
}
