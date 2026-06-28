//! VC-2.1 / VC-2.2 / VC-2.3 — snapshot equivalence, restore, and fallback (offline).
//!
//! This is the integration sink for the SNAPSHOT EQUIVALENCE, RESTORE, and
//! FALLBACK properties. It builds a synthetic recorded log (no model calls,
//! no credentials), writes snapshots at chosen ticks via `SnapshotStore`, and
//! asserts three deterministic equivalences:
//!
//! - **VC-2.1 (Happy).**
//!   `digest(snapshot_at(log, t)) == digest(fold_log(genesis, log[..=t]))` for
//!   every target tick. The snapshot serializes the entire `World` including
//!   `Resources.ids`, `rng`, `wall`, and `clock`; structural equality of `world`
//!   follows from digest equality (Inv 3, 8).
//! - **VC-2.2 (Happy).**
//!   `restore(at)` — nearest snapshot ≤ at plus a tail fold — produces a `World`
//!   BYTE-IDENTICAL (serde `to_vec`) to a full fold from genesis, across EVERY
//!   target tick. Targets cover: a tick at a snapshot, a tick between two
//!   snapshots, and a tail-only target PAST the last snapshot. (Inv 8, 9.)
//! - **VC-2.3 (Negative).**
//!   A byte-tampered snapshot file is REJECTED by the store loader (digest
//!   mismatch); `nearest_at_or_before` falls back to the next-earlier valid
//!   snapshot (or genesis when none survives). `restore` over the corrupted store
//!   still produces the SAME `World` as a full fold — NO data loss (Inv 4, 9).
//!
//! All proofs run OFFLINE: no model client is reached, no network calls are made.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 3 (snapshot = fold-prefix), Inv 4
//! (digest integrity), Inv 8 (recorded seed), Inv 9 (log is single source).

use std::collections::BTreeMap;

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{Command, fingerprint_call};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::lifecycle::LifecycleEvent;
use rubberdux::agent::world::replay::{fold_log, genesis_from_log, restore};
use rubberdux::agent::world::snapshot::{capture, snapshot_at, SnapshotStore};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};

// ---------------------------------------------------------------------------
// Shared constants and genesis builder
// ---------------------------------------------------------------------------

/// The hidden RNG seed encoded in the synthetic log's `SessionStarted` header.
const SEED: u64 = 55;

/// A world-default `ModelConfig` whose `model` id is inert — no real call is made.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-snapshot-restore-sink".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// The fresh `World` a session starts from: tick 0, one primary `Idle` entity,
/// `Resources` reseeded from `seed`. Mirrors the canonical genesis every replay
/// harness builds — the shell owns genesis; no P0 System creates it.
fn genesis(seed: u64, model: &ModelConfig) -> World {
    let mut world = World::new(0, Resources::new(seed, model.clone()));
    world.entities.insert(
        0,
        Components {
            identity: Identity::Primary,
            lineage: Lineage { parent: None, depth: 0 },
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

// ---------------------------------------------------------------------------
// Log construction helpers
// ---------------------------------------------------------------------------

fn session_started() -> Event {
    Event {
        origin: Origin::System,
        edge: 0,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed: SEED,
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

/// Placeholder `ModelResponded` with an empty fingerprint; call
/// `stamp_fingerprints` after assembling the full log to fill them.
fn model_responded_placeholder(at: u64, cmd: CmdId, text: &str, model: &ModelConfig) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            blocks: vec![Block::Text { text: text.into() }],
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: model.model.clone(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

/// Stamp every `ModelResponded` in `events` with the fingerprint the live driver
/// would record for the request the reducer re-emits for that `cmd` (Inv 7). This
/// ensures the replay driver REUSES each recorded result instead of diverging.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig) {
    let mut world = genesis_from_log(events, |seed| genesis(seed, model))
        .expect("genesis for the fingerprint stamping pass");
    let mut by_cmd: BTreeMap<CmdId, Fingerprint> = BTreeMap::new();

    for ev in events.iter() {
        let (next, commands) = tick(&world, ev);
        world = next;
        for command in &commands {
            if let Command::CallModel {
                cmd, messages, tools, params, ..
            } = command
            {
                let fp = fingerprint_call(messages, tools, params)
                    .expect("fingerprint a CallModel request");
                by_cmd.insert(*cmd, fp);
            }
        }
    }
    for ev in events.iter_mut() {
        if let LogicalInput::ModelResponded { cmd, fingerprint, .. } = &mut ev.input
            && let Some(fp) = by_cmd.get(cmd)
        {
            *fingerprint = fp.clone();
        }
    }
}

/// Build the canonical synthetic log: `SessionStarted` + two turns (each a
/// `UserMessage` followed by a `ModelResponded`). Fingerprints are stamped in
/// place so the replay driver reuses every recorded result.
///
/// Tick layout:
/// - 0: `SessionStarted`
/// - 1: `UserMessage` ("first question")
/// - 2: `ModelResponded` (cmd 0, "first answer")
/// - 3: `UserMessage` ("second question")
/// - 4: `ModelResponded` (cmd 1, "second answer")
fn build_log(model: &ModelConfig) -> Vec<Event> {
    let mut events = vec![
        session_started(),
        user_message(1, "first question"),
        model_responded_placeholder(2, 0, "first answer", model),
        user_message(3, "second question"),
        model_responded_placeholder(4, 1, "second answer", model),
    ];
    stamp_fingerprints(&mut events, model);
    events
}

// ---------------------------------------------------------------------------
// VC-2.1 — digest(snapshot_at(log, t)) == digest(fold_log(genesis, log[..=t]))
// ---------------------------------------------------------------------------

/// **VC-2.1** Proves snapshot≡fold for EVERY tick in the synthetic log.
///
/// For each target tick `t`, `snapshot_at(events, build_genesis, t)` captures the
/// world the log converges to AT tick `t`. The resulting `snapshot.world` must be
/// BYTE-IDENTICAL (same `serde_json::to_vec` output) to `fold_log(genesis,
/// events[..=t])`. Because `Snapshot.digest` is `hex(SHA-256(serde_json::to_vec(&world)))`,
/// this byte-identity also proves `snapshot.digest == digest(fold_log(..=t))` (Inv 3/8).
///
/// Structural equality of `ids`, `rng`, `wall`, and `clock` is tested by asserting
/// the ENTIRE World serde-round-trips byte-identically — not by inspecting fields.
#[test]
fn vc_2_1_snapshot_at_is_byte_identical_to_fold_log_at_every_tick() {
    let model = offline_model();
    let events = build_log(&model);

    for target in 0u64..=4 {
        let snap = snapshot_at(&events, |seed| genesis(seed, &model), target)
            .expect("snapshot_at must succeed on a valid log");

        // Reproduce what fold_log(genesis, events[..=target]) gives.
        let prefix: Vec<Event> = events.iter().filter(|e| e.at <= target).cloned().collect();
        let genesis_world =
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis from log");
        let expected = fold_log(genesis_world, &prefix);

        let snap_bytes = serde_json::to_vec(&snap.world).expect("serialize snapshot world");
        let expected_bytes = serde_json::to_vec(&expected).expect("serialize expected world");

        assert_eq!(
            snap_bytes, expected_bytes,
            "snapshot_at({target}).world must be byte-identical to fold_log(log[..={target}])"
        );
        assert_eq!(
            snap.world, expected,
            "snapshot_at({target}).world must be structurally equal to fold_log(log[..={target}])"
        );

        // Prove digest equivalence: capture a snapshot from the fold world and compare
        // digests — both are `hex(SHA-256(serde_json::to_vec(&world)))` over the same bytes.
        let expected_snap = capture(expected.clone());
        assert_eq!(
            snap.digest, expected_snap.digest,
            "digest(snapshot_at({target})) must equal digest(fold_log(log[..={target}]))"
        );
        assert!(
            snap.validate(),
            "snapshot_at({target}) must validate its own digest"
        );
    }
}

// ---------------------------------------------------------------------------
// VC-2.2 — restore(at) ≡ full fold from genesis, for multiple targets
// ---------------------------------------------------------------------------

/// **VC-2.2** Proves `restore(at)` is byte-identical to a full fold from genesis
/// across EVERY target tick, including ticks covered by a snapshot, ticks BETWEEN
/// two snapshots, and a tick PAST the last snapshot (tail-only reconstruct).
///
/// Snapshot layout (written to a temp store):
/// - snapshot at tick 0 (genesis / `SessionStarted`)
/// - snapshot at tick 2 (after turn-1 model response)
///
/// Targets 0–4 exercise all paths:
/// - 0: exact-match snapshot (tick 0 ≤ 0 → no tail to fold)
/// - 1: snapshot at tick 0, tail = event at tick 1
/// - 2: exact-match snapshot (tick 2 ≤ 2 → no tail to fold)
/// - 3: snapshot at tick 2, tail = event at tick 3
/// - 4: snapshot at tick 2, tail = events at ticks 3–4 (PAST the last snapshot)
#[test]
fn vc_2_2_restore_at_every_target_is_byte_identical_to_full_fold() {
    let model = offline_model();
    let events = build_log(&model);

    // Write snapshots at ticks 0 and 2.
    let dir = tempfile::tempdir().expect("create tempdir for SnapshotStore");
    let store = SnapshotStore::new(dir.path().join("snapshots"));
    for t in [0u64, 2u64] {
        let snap = snapshot_at(&events, |seed| genesis(seed, &model), t)
            .expect("snapshot_at must succeed");
        store.write(&snap).expect("write snapshot");
    }

    // A complete log has no outstanding dispatches; the lifecycle is empty.
    let lifecycle: Vec<LifecycleEvent> = Vec::new();

    for target in 0u64..=4 {
        // The expected world: fold the prefix up to `target` from genesis.
        let prefix: Vec<Event> = events.iter().filter(|e| e.at <= target).cloned().collect();
        let genesis_world =
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis from log");
        let expected = fold_log(genesis_world, &prefix);

        let mut log = MemoryEventLog::new();
        let restored = restore(
            &store,
            &events,
            &lifecycle,
            |seed| genesis(seed, &model),
            target,
            &mut log,
        )
        .expect("restore must succeed");

        assert_eq!(
            serde_json::to_vec(&restored).expect("serialize restored"),
            serde_json::to_vec(&expected).expect("serialize expected"),
            "restore({target}) must be byte-identical to a full fold from genesis"
        );
        assert_eq!(
            restored, expected,
            "restore({target}) must be structurally equal to a full fold from genesis"
        );
        assert!(
            log.load().expect("load resume log").is_empty(),
            "a complete log has nothing outstanding to reconcile at target {target}"
        );
    }
}

/// **VC-2.2 (no-snapshot path)** When no snapshots exist, `restore` falls back to
/// genesis and folds the WHOLE prefix — still byte-identical to a full fold (Inv 9).
#[test]
fn vc_2_2_restore_without_snapshots_falls_back_to_full_fold() {
    let model = offline_model();
    let events = build_log(&model);

    let dir = tempfile::tempdir().expect("create tempdir for empty SnapshotStore");
    let store = SnapshotStore::new(dir.path().join("snapshots")); // never written
    let lifecycle: Vec<LifecycleEvent> = Vec::new();

    let genesis_world =
        genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis from log");
    let expected = fold_log(genesis_world, &events);

    let mut log = MemoryEventLog::new();
    let restored = restore(
        &store,
        &events,
        &lifecycle,
        |seed| genesis(seed, &model),
        4,
        &mut log,
    )
    .expect("restore with no snapshots must fall back to a full fold");

    assert_eq!(
        serde_json::to_vec(&restored).expect("serialize restored"),
        serde_json::to_vec(&expected).expect("serialize expected"),
        "restore with an empty store must equal a full fold from genesis"
    );
}

// ---------------------------------------------------------------------------
// VC-2.3 — a tampered snapshot is rejected; restore still equals full fold
// ---------------------------------------------------------------------------

/// **VC-2.3** Proves that a byte-tampered snapshot is REJECTED by the store loader
/// (digest mismatch) and that `nearest_at_or_before` falls back to the next-earlier
/// valid snapshot (or `None` → genesis), with NO data loss: `restore` over the
/// corrupted store still produces the SAME `World` as a full fold from genesis.
///
/// Three sub-cases are exercised:
/// 1. Only the LATER snapshot (tick 2) is corrupted → falls back to tick-0 snapshot;
///    the final World still equals full fold at tick 4.
/// 2. BOTH snapshots (ticks 0 and 2) are corrupted → `nearest_at_or_before` returns
///    `None`; restore falls back to genesis + full tail; same final World.
/// 3. Corrupt files must NOT be deleted (the log is the source of truth, Inv 9).
#[test]
fn vc_2_3_tampered_snapshot_rejected_and_restore_falls_back_no_data_loss() {
    let model = offline_model();
    let events = build_log(&model);

    // Write valid snapshots at ticks 0 and 2.
    let dir = tempfile::tempdir().expect("create tempdir");
    let snapshots_dir = dir.path().join("snapshots");
    let store = SnapshotStore::new(&snapshots_dir);

    let snap0 = snapshot_at(&events, |seed| genesis(seed, &model), 0).expect("snapshot_at(0)");
    let snap2 = snapshot_at(&events, |seed| genesis(seed, &model), 2).expect("snapshot_at(2)");
    store.write(&snap0).expect("write snap0");
    store.write(&snap2).expect("write snap2");

    // The expected final World (full fold at tick 4).
    let genesis_world =
        genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis from log");
    let expected_full = fold_log(genesis_world, &events);

    let lifecycle: Vec<LifecycleEvent> = Vec::new();

    // --- Case 1: only the tick-2 snapshot is corrupted ----------------------

    // Flip a byte in the middle of the tick-2 snapshot file.
    let path2 = snapshots_dir.join("2.snapshot.json");
    let mut bytes2 = std::fs::read(&path2).expect("read snap2 file");
    let mid2 = bytes2.len() / 2;
    bytes2[mid2] ^= 0xAA;
    std::fs::write(&path2, &bytes2).expect("write corrupted snap2");

    // nearest_at_or_before(4) must skip the corrupt tick-2 file and return tick-0.
    let nearest = store
        .nearest_at_or_before(4)
        .expect("nearest_at_or_before with one corrupt snapshot");
    let nearest_snap = nearest.expect("must fall back to the valid tick-0 snapshot");
    assert_eq!(
        nearest_snap.tick, 0,
        "must fall back to tick-0 when tick-2 snapshot is corrupt"
    );

    // restore(4) over the partially-corrupted store must still equal the full fold.
    let mut log1 = MemoryEventLog::new();
    let restored1 = restore(
        &store,
        &events,
        &lifecycle,
        |seed| genesis(seed, &model),
        4,
        &mut log1,
    )
    .expect("restore with one corrupt snapshot must succeed via fallback");

    assert_eq!(
        serde_json::to_vec(&restored1).expect("serialize"),
        serde_json::to_vec(&expected_full).expect("serialize"),
        "restore must equal full fold even when tick-2 snapshot is corrupt (no data loss)"
    );

    // The corrupted file must NOT have been deleted (Inv 9).
    assert!(
        path2.exists(),
        "a corrupt snapshot file must not be deleted by the loader (Inv 9)"
    );

    // --- Case 2: BOTH snapshots corrupted → falls back to genesis ------------

    let path0 = snapshots_dir.join("0.snapshot.json");
    let mut bytes0 = std::fs::read(&path0).expect("read snap0 file");
    let mid0 = bytes0.len() / 2;
    bytes0[mid0] ^= 0xAA;
    std::fs::write(&path0, &bytes0).expect("write corrupted snap0");

    // nearest_at_or_before(4) must return None.
    let nearest_none = store
        .nearest_at_or_before(4)
        .expect("nearest_at_or_before when all corrupt");
    assert!(
        nearest_none.is_none(),
        "must return None when all snapshots are corrupt (caller falls back to genesis)"
    );

    // restore(4) over an all-corrupted store must still equal the full fold.
    let mut log2 = MemoryEventLog::new();
    let restored2 = restore(
        &store,
        &events,
        &lifecycle,
        |seed| genesis(seed, &model),
        4,
        &mut log2,
    )
    .expect("restore with all corrupt snapshots must succeed via genesis fallback");

    assert_eq!(
        serde_json::to_vec(&restored2).expect("serialize"),
        serde_json::to_vec(&expected_full).expect("serialize"),
        "restore must equal full fold even when ALL snapshots are corrupt (no data loss)"
    );

    // Both corrupt files must still be on disk (Inv 9).
    assert!(path0.exists(), "corrupt tick-0 file must not be deleted");
    assert!(path2.exists(), "corrupt tick-2 file must not be deleted");
}
