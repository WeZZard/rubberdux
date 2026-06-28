//! VC-3.1 / VC-3.2 / VC-3.3 — pruning, log protection, and dead-branch GC (offline).
//!
//! This is the integration sink for the PRUNING, LOG-PROTECTION, and
//! DEAD-BRANCH-GC properties. All proofs run offline against synthetic
//! recorded logs + on-disk fixtures built in a tempdir: no model client is
//! reached, no credentials are required.
//!
//! - **VC-3.1 (Happy).** After retention to K, only the latest K snapshots
//!   remain on disk AND every retained snapshot's predecessor (an evicted tick)
//!   is still regenerable by replaying the event log from genesis: the World at
//!   each evicted tick is byte-identical to the deleted snapshot. (Inv 9, 11.)
//! - **VC-3.2 (Edge).** A fully-covered sealed log segment is MOVED (never
//!   deleted) to `cold/` via [`SegmentedEventLog::tier_to_cold`]; after tiering,
//!   [`SegmentedEventLog::load`] returns the full ordered event sequence
//!   BYTE-IDENTICALLY to the pre-tiering load; no segment needed for
//!   regeneration is ever deleted; the moved segment is absent from the hot
//!   tier and present in `cold/`. (Inv 9.)
//! - **VC-3.3 (Edge).** A dead branch (explicitly dropped/tombstoned and
//!   unreachable) is swept by [`reclaim_branches`] while a reachable (pinned)
//!   branch, its branch-local snapshots, the shared parent prefix (the MAIN log),
//!   and the branch index are all left intact; only the dead branch's own
//!   divergent tail and branch-local snapshots are removed. (Inv 11, 20.)
//!
//! See docs/agent/world/ecs-runtime.md — PRUNE / snapshot retention (Inv 9/11),
//! cold-storage tiering (Inv 9), dead-branch reachability GC (Invariant 20).

use rubberdux::agent::world::branch::{fork, persist_branch, Edit, EditKind};
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::event_log::EventLog;
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::History;
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};
use rubberdux::agent::world::reclaim::{drop_branch, pin, reclaim_branches, Roots};
use rubberdux::agent::world::replay::fold_log;
use rubberdux::agent::world::retention::evictable_snapshots;
use rubberdux::agent::world::segment::{cold_storable, SegmentedEventLog};
use rubberdux::agent::world::snapshot::{capture, snapshot_at, SnapshotStore};
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};

// ---------------------------------------------------------------------------
// Shared test fixtures
// ---------------------------------------------------------------------------

/// RNG seed used in every synthetic log in this sink.
const SEED: u64 = 31;

/// An offline `ModelConfig` whose `model` id is inert — no real call is made.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-prune-reclaim-sink".into(),
        max_tokens: 512,
        effort: Effort::Low,
    }
}

/// The fresh `World` a session starts from: tick 0, one primary `Idle` entity,
/// `Resources` reseeded from `seed`. Mirrors the canonical genesis every replay
/// harness builds — the shell owns genesis; no P0 System creates it.
fn make_genesis(seed: u64) -> World {
    let model = offline_model();
    let mut world = World::new(0, Resources::new(seed, model));
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
            gate: rubberdux::agent::world::gates::EntityGate::default(),
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

/// A synthetic `SessionStarted` event at tick 0.
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

/// A synthetic `UserMessage` event.
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

/// A small canonical recorded log: a genesis header plus several user messages.
fn sample_log() -> Vec<Event> {
    vec![
        session_started(SEED),
        user_message(1, "alpha"),
        user_message(2, "beta"),
        user_message(3, "gamma"),
        user_message(4, "delta"),
        user_message(5, "epsilon"),
    ]
}

// ---------------------------------------------------------------------------
// VC-3.1 — pruning to K: only latest K remain; every evicted tick is
//           regenerable byte-identically from the log.
// ---------------------------------------------------------------------------

/// After pruning with `keep = K`, only the latest K snapshots remain on disk.
/// For each evicted tick, the World can be regenerated by `fold_log` and the
/// resulting World is byte-identical to what the now-deleted snapshot held.
///
/// This proves that pruning loses no unique data — the event log is the single
/// source of truth (Inv 9/11; VC-3.1).
#[test]
fn vc3_1_pruning_keeps_latest_k_and_evicted_are_regenerable() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let snapshots_dir = dir.path().join("snapshots");
    let store = SnapshotStore::new(&snapshots_dir);

    let events = sample_log();

    // Capture and persist snapshots at ticks 1 through 5.
    let mut original_snapshots = Vec::new();
    for tick in 1u64..=5 {
        let snap = snapshot_at(&events, make_genesis, tick)
            .unwrap_or_else(|_| panic!("snapshot_at must succeed at tick {tick}"));
        store.write(&snap).expect("write snapshot");
        original_snapshots.push(snap);
    }

    // Prune keeping only the latest 2. The live tail is at tick 5.
    let keep: u32 = 2;
    let live_tail: u64 = 5;
    let evicted = store.prune(keep, live_tail).expect("prune must succeed");

    // Exactly 3 ticks must have been evicted (5 total – 2 kept = 3).
    let mut evicted_sorted = evicted.clone();
    evicted_sorted.sort_unstable();
    assert_eq!(
        evicted_sorted,
        vec![1u64, 2, 3],
        "ticks 1, 2, 3 must be evicted when keep=2 and live_tail=5"
    );

    // Verify on-disk state: ticks 4 and 5 are retained; ticks 1, 2, 3 are gone.
    for tick in [1u64, 2, 3] {
        assert!(
            !snapshots_dir.join(format!("{tick}.snapshot.json")).exists(),
            "snapshot at tick {tick} must be deleted by prune"
        );
    }
    for tick in [4u64, 5] {
        assert!(
            snapshots_dir.join(format!("{tick}.snapshot.json")).exists(),
            "snapshot at tick {tick} must be retained"
        );
    }

    // Retained-successor regenerability: for each evicted tick, replay from
    // genesis using the event log and assert the result is byte-identical to
    // the original snapshot.World. This proves that the deleted snapshot holds
    // no unique data — a replay always reproduces it (Inv 9/11).
    let genesis_world = make_genesis(SEED);
    for (i, tick) in [1u64, 2, 3].iter().enumerate() {
        let original_snap = &original_snapshots[i]; // snapshots are indexed 0..=4 for ticks 1..=5
        let prefix: Vec<Event> = events.iter().filter(|e| e.at <= *tick).cloned().collect();
        let replayed_world = fold_log(genesis_world.clone(), &prefix);

        assert_eq!(
            serde_json::to_vec(&replayed_world).expect("serialize replayed"),
            serde_json::to_vec(&original_snap.world).expect("serialize original snap world"),
            "the World at evicted tick {tick} must be regenerable byte-identically from the log"
        );
    }

    // Confirm that the retained snapshots at ticks 4 and 5 are still valid.
    for tick in [4u64, 5] {
        let loaded = store
            .nearest_at_or_before(tick)
            .expect("nearest_at_or_before")
            .expect("retained snapshot must still be loadable");
        assert_eq!(loaded.tick, tick, "retained snapshot tick must match");
        assert!(loaded.validate(), "retained snapshot must still pass validation");
    }
}

/// `keep = 0` (unbounded retention): no snapshot is ever evictable; all five
/// snapshots survive a prune pass. (Inv 11 boundary case.)
#[test]
fn vc3_1_keep_zero_means_unbounded_retention() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let events = sample_log();

    for tick in 1u64..=5 {
        let snap = snapshot_at(&events, make_genesis, tick).expect("snapshot_at");
        store.write(&snap).expect("write snapshot");
    }

    let evicted = store.prune(0, 5).expect("prune with keep=0");
    assert!(
        evicted.is_empty(),
        "keep=0 must evict nothing; all snapshots must be retained"
    );

    // All five snapshot files must still exist.
    let ticks_on_disk = store.list_ticks().expect("list_ticks");
    let mut ticks_sorted = ticks_on_disk.clone();
    ticks_sorted.sort_unstable();
    assert_eq!(
        ticks_sorted,
        vec![1u64, 2, 3, 4, 5],
        "all five snapshots must remain when keep=0"
    );
}

/// When the snapshot count does not exceed `keep`, no snapshot is evicted.
/// (Boundary case: having fewer snapshots than the keep window.)
#[test]
fn vc3_1_fewer_snapshots_than_keep_evicts_nothing() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let events = sample_log();

    // Write only 2 snapshots but keep = 5.
    for tick in [2u64, 4] {
        let snap = snapshot_at(&events, make_genesis, tick).expect("snapshot_at");
        store.write(&snap).expect("write snapshot");
    }

    let evicted = store.prune(5, 4).expect("prune");
    assert!(
        evicted.is_empty(),
        "fewer snapshots than keep must evict nothing"
    );
}

// ---------------------------------------------------------------------------
// VC-3.2 — cold-storage tiering: fully-covered segment is moved (not deleted);
//           load() stays byte-identical before and after tiering; no segment
//           needed for regeneration is deleted.
// ---------------------------------------------------------------------------

/// Build a segmented log with two sealed segments plus an active one. Cold-tier
/// the fully-covered sealed segments. Assert:
///
/// 1. The tiered segments are ABSENT from the hot tier and PRESENT in `cold/`.
/// 2. `load()` returns the full event sequence BYTE-IDENTICALLY before and after.
/// 3. No segment file is deleted — only moved.
/// 4. The active (open) segment is NEVER tiered.
///
/// (Inv 9; VC-3.2.)
#[test]
fn vc3_2_tiering_moves_not_deletes_and_load_stays_byte_identical() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let root = dir.path().join("world");

    // Build two sealed segments ([0..3], [4..5]) and one active segment ([6..]).
    let mut log = SegmentedEventLog::open(&root).expect("open segmented log");
    let events = sample_log();

    // Append ticks 0–3 into the initial segment (world-events).
    for e in events.iter().filter(|e| e.at <= 3) {
        log.append(e).expect("append event");
    }
    // Roll at tick 4: seals [0..3] as world-events, opens seg-4.
    log.seal_and_roll(4).expect("seal and roll at 4");

    // Append ticks 4–5 into seg-4.
    for e in events.iter().filter(|e| e.at >= 4 && e.at <= 5) {
        log.append(e).expect("append event");
    }
    // Roll at tick 6: seals [4..5] as seg-4, opens seg-6.
    log.seal_and_roll(6).expect("seal and roll at 6");

    // Append tick-6+ into the active seg-6 (simulate more activity).
    log.append(&user_message(6, "zeta")).expect("append tick 6");

    // Capture the full log BEFORE tiering as the reference.
    let before: Vec<Event> = log.load().expect("load before tiering");
    let before_bytes = serde_json::to_vec(&before).expect("serialize before");

    // Three segments now: world-events [0..3] (sealed, hot), seg-4 [4..5]
    // (sealed, hot), seg-6 [6..] (active/open).
    assert_eq!(
        log.manifest().segments.len(),
        3,
        "must have exactly 3 segments before tiering"
    );

    // floor = 6: both sealed segments (last_tick 3 and 5) are fully covered.
    let floor: u64 = 6;
    let storable = cold_storable(log.manifest(), floor);
    assert_eq!(
        storable.len(),
        2,
        "both sealed hot segments must be cold-storable at floor={floor}"
    );

    // Tier the cold-storable set.
    let moved = log.tier_cold_storable(floor).expect("tier cold-storable");
    assert_eq!(
        moved.len(),
        2,
        "tier_cold_storable must move exactly the cold-storable set"
    );

    // 1. Each moved segment must be ABSENT from the hot tier and PRESENT in cold/.
    for id in &moved {
        let hot_path = root.join(format!("{}.jsonl", id.0));
        let cold_path = root.join("cold").join(format!("{}.jsonl", id.0));

        assert!(
            !hot_path.exists(),
            "tiered segment '{}' must be absent from the hot tier",
            id.0
        );
        assert!(
            cold_path.exists(),
            "tiered segment '{}' must be present in cold/",
            id.0
        );
    }

    // 2. load() is BYTE-IDENTICAL before and after tiering (the critical invariant).
    let after: Vec<Event> = log.load().expect("load after tiering");
    let after_bytes = serde_json::to_vec(&after).expect("serialize after");

    assert_eq!(
        after, before,
        "load() must return the same event sequence before and after tiering"
    );
    assert_eq!(
        after_bytes, before_bytes,
        "load() must be BYTE-IDENTICAL before and after tiering (VC-3.2)"
    );

    // 3. No segment file is deleted: the hot segment files moved to cold/, and the
    //    active (open) segment stays in the hot tier.
    let active_seg_id = log
        .manifest()
        .segments
        .last()
        .expect("manifest must have at least one segment")
        .id
        .clone();
    assert!(
        root.join(format!("{}.jsonl", active_seg_id.0)).exists(),
        "the open (active) segment must remain in the hot tier"
    );
    assert!(
        root.join("cold").exists(),
        "the cold/ directory must exist after tiering"
    );

    // 4. No segment needed for regeneration is deleted — every cold segment is
    //    still loadable through a fresh handle, which resolves to the cold path.
    let reopened = SegmentedEventLog::open(&root).expect("reopen after tiering");
    let reloaded: Vec<Event> = reopened.load().expect("load via reopened handle");
    assert_eq!(
        reloaded, before,
        "a reopened SegmentedEventLog must load the full log including cold-tiered segments"
    );
}

/// A legacy single-file log (no `manifest.json`) loads as ONE implicit hot
/// segment and its events survive into a segmented context: tiering nothing at
/// floor=0, loading identically (VC-3.2 — M1 log compatibility, Inv 9).
#[test]
fn vc3_2_legacy_single_file_loads_unchanged_and_is_not_tiered_at_zero_floor() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let root = dir.path().join("world");
    std::fs::create_dir_all(&root).expect("create root");

    let events = sample_log();
    let mut blob = String::new();
    for e in &events {
        blob.push_str(&serde_json::to_string(e).expect("encode event"));
        blob.push('\n');
    }
    std::fs::write(root.join("world-events.jsonl"), &blob).expect("write legacy log");

    // Open as a segmented log and confirm legacy file loads unchanged.
    let mut log = SegmentedEventLog::open(&root).expect("open legacy log");
    let loaded = log.load().expect("load legacy log");
    assert_eq!(
        loaded, events,
        "legacy single-file log must load byte-identically"
    );

    // floor=0 tiers nothing (strict `last_tick < floor`; 0 < 0 is false).
    let moved = log.tier_cold_storable(0).expect("tier with floor=0");
    assert!(
        moved.is_empty(),
        "floor=0 must tier no segment (nothing can have last_tick < 0)"
    );

    // The log still loads identically after the no-op tiering call.
    let after = log.load().expect("load after no-op tiering");
    assert_eq!(
        after, events,
        "load must be unchanged after a no-op tiering call"
    );
}

// ---------------------------------------------------------------------------
// VC-3.3 — dead-branch GC: reclaim deletes ONLY the dead branch's own tail +
//           snapshots; reachable branch, shared parent prefix, and index intact.
// ---------------------------------------------------------------------------

/// Build two branches off the MAIN log, one dead (tombstoned) and one reachable
/// (pinned). Each has a branch-local snapshot. After `reclaim_branches`:
///
/// 1. Only the dead branch's own directory (tail + snapshots) is removed.
/// 2. The pinned branch's tail and branch-local snapshot are intact.
/// 3. The shared parent prefix (MAIN `world-events.jsonl`) is untouched.
/// 4. The branch index (`branches/index.jsonl`) survives.
///
/// (Inv 11, 20; VC-3.3.)
#[test]
fn vc3_3_dead_branch_reclaimed_reachable_and_prefix_retained() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let session_dir = dir.path();

    // The shared parent prefix — the MAIN log — must never be touched.
    let main_log_content = b"main log line\n";
    std::fs::write(session_dir.join("world-events.jsonl"), main_log_content)
        .expect("write MAIN log");

    // Build the MAIN event log to fork from.
    let log = sample_log();

    // Helper: fork a branch, persist it, and plant a branch-local snapshot.
    let plant_branch = |text: &str| -> rubberdux::agent::world::branch::BranchId {
        let edit = Edit {
            at_tick: 1,
            input: LogicalInput::UserMessage { to: 0, text: text.into() },
            kind: EditKind::Replace,
        };
        let (id, branch) =
            fork(&log, 1, edit.clone()).expect("fork must succeed for an exogenous edit");
        persist_branch(session_dir, &rubberdux::agent::world::branch::BranchId::MAIN, 1, &edit, &branch, 1_000)
            .expect("persist_branch must succeed");

        // Plant a branch-local snapshot in branches/<id>/snapshots/.
        let snap_dir = session_dir
            .join("branches")
            .join(&id.0)
            .join("snapshots");
        std::fs::create_dir_all(&snap_dir).expect("create branch snapshots dir");
        std::fs::write(snap_dir.join("1.snapshot.json"), b"{}").expect("write branch snapshot");

        id
    };

    let dead_id = plant_branch("dead-branch-message");
    let live_id = plant_branch("live-branch-message");

    // Drop (tombstone) the dead branch; pin the live branch.
    drop_branch(session_dir, &dead_id).expect("drop_branch must succeed");
    pin(session_dir, &live_id, true).expect("pin must succeed");

    // Run the reclaim pass with default roots (MAIN session, grace=0 → age-based
    // reclamation disabled; only explicit tombstones qualify).
    let swept =
        reclaim_branches(session_dir, &Roots::default(), 10_000).expect("reclaim_branches");

    // 1. Only the dead branch must have been swept.
    assert_eq!(
        swept,
        vec![dead_id.clone()],
        "only the tombstoned branch must be swept; the pinned branch must survive"
    );

    let branches = session_dir.join("branches");

    // 2. Dead branch's own directory (tail + snapshots) must be gone.
    assert!(
        !branches.join(&dead_id.0).exists(),
        "dead branch's own directory (tail + branch-local snapshots) must be removed (VC-3.3)"
    );

    // 3. Pinned branch's tail and branch-local snapshot must be intact.
    assert!(
        branches.join(&live_id.0).join("world-events.jsonl").exists(),
        "the pinned branch's event log tail must be retained"
    );
    assert!(
        branches
            .join(&live_id.0)
            .join("snapshots")
            .join("1.snapshot.json")
            .exists(),
        "the pinned branch's branch-local snapshot must be retained"
    );

    // 4. Shared parent prefix (MAIN log) must be byte-identical — never touched.
    let main_log_after =
        std::fs::read(session_dir.join("world-events.jsonl")).expect("read MAIN log");
    assert_eq!(
        main_log_after, main_log_content,
        "the shared parent prefix (MAIN log) must not be altered by reclamation (Inv 20)"
    );

    // 5. Branch index must survive.
    assert!(
        branches.join("index.jsonl").exists(),
        "branches/index.jsonl must survive the reclaim pass"
    );
}

/// Aged (but not dropped) branches within the grace window are retained.
/// Only a branch whose age exceeds the grace window IS reclaimed when tombstone
/// is also set. Branches with `grace=0` are ONLY reclaimed when tombstoned.
#[test]
fn vc3_3_grace_zero_retains_non_tombstoned_branches() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let session_dir = dir.path();

    let log = sample_log();

    // Create one branch that is NOT tombstoned.
    let edit = Edit {
        at_tick: 1,
        input: LogicalInput::UserMessage { to: 0, text: "keep-me".into() },
        kind: EditKind::Replace,
    };
    let (id, branch) =
        fork(&log, 1, edit.clone()).expect("fork");
    persist_branch(
        session_dir,
        &rubberdux::agent::world::branch::BranchId::MAIN,
        1,
        &edit,
        &branch,
        0, // created_wall = 0
    )
    .expect("persist_branch");

    // Roots with grace=0: age-based reclamation disabled; only tombstones qualify.
    let roots = Roots::default(); // grace=0, live_session=MAIN
    let swept =
        reclaim_branches(session_dir, &roots, 1_000_000).expect("reclaim_branches with grace=0");

    assert!(
        swept.is_empty(),
        "a non-tombstoned branch must be retained when grace=0 (VC-3.3)"
    );

    // Verify the branch directory is still present.
    assert!(
        session_dir.join("branches").join(&id.0).exists(),
        "the non-tombstoned branch's directory must survive"
    );
}

/// A reachable branch (parent of a pinned child) is never reclaimed, even when
/// the parent is itself tombstoned — the upward reachability closure protects it.
/// This exercises the "parent-of-retained" reachability root (Inv 20; VC-3.3).
#[test]
fn vc3_3_parent_of_retained_branch_is_never_reclaimed() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let session_dir = dir.path();

    let log = sample_log();

    // Persist a "parent" branch from MAIN.
    let edit_parent = Edit {
        at_tick: 1,
        input: LogicalInput::UserMessage { to: 0, text: "parent-branch".into() },
        kind: EditKind::Replace,
    };
    let (parent_id, parent_branch) = fork(&log, 1, edit_parent.clone()).expect("fork parent");
    persist_branch(
        session_dir,
        &rubberdux::agent::world::branch::BranchId::MAIN,
        1,
        &edit_parent,
        &parent_branch,
        0,
    )
    .expect("persist parent branch");

    // Drop (tombstone) the parent — it would otherwise be dead.
    drop_branch(session_dir, &parent_id).expect("drop parent");

    // Persist a "child" branch that is pinned. In a real fork it would fork off
    // the parent branch; here we simulate it by building a descriptor via the
    // reclaim pin mutator rather than a full nested fork, to keep the test
    // self-contained. However, to exercise the actual `reachable` upward-closure
    // path, we need the descriptor to carry `parent = parent_id`. Because
    // `persist_branch` does not support reparenting, we use `fork` from the
    // original log and rely on the pure `reachable` + `reclaimable` functions
    // directly (the IO `reclaim_branches` path is exercised in the previous test).
    use rubberdux::agent::world::branch::BranchDescriptor;
    use rubberdux::agent::world::inputs::Fingerprint;
    use rubberdux::agent::world::reclaim::{reclaimable, reachable};

    let child_id =
        rubberdux::agent::world::branch::BranchId("child-pinned-0000".into());

    // Build the descriptors in memory — no extra IO needed for the pure proof.
    let parent_desc = BranchDescriptor {
        id: parent_id.clone(),
        parent: rubberdux::agent::world::branch::BranchId::MAIN,
        fork_tick: 1,
        edit_summary: "parent".into(),
        created_wall: 0,
        prefix_digest: Fingerprint(String::new()),
        pin: false,
        tombstone: true, // dropped
    };
    let child_desc = BranchDescriptor {
        id: child_id.clone(),
        parent: parent_id.clone(), // child's parent is the tombstoned parent
        fork_tick: 2,
        edit_summary: "child-pinned".into(),
        created_wall: 0,
        prefix_digest: Fingerprint(String::new()),
        pin: true, // pinned → reachability root
        tombstone: false,
    };
    let branches = vec![parent_desc, child_desc.clone()];

    // The pinned child is reachable; its tombstoned parent is ALSO reachable
    // (the upward closure); MAIN is always reachable.
    let roots = Roots::default();
    let reached = reachable(&branches, &roots);
    assert!(
        reached.contains(&child_id),
        "the pinned child must be reachable"
    );
    assert!(
        reached.contains(&parent_id),
        "the tombstoned parent of a pinned child must be reachable via upward closure (Inv 20)"
    );
    assert!(
        reached.contains(&rubberdux::agent::world::branch::BranchId::MAIN),
        "MAIN is always reachable"
    );

    // Neither is reclaimable — neither appears in the dead set.
    let dead = reclaimable(&branches, &roots, 1_000_000);
    assert!(
        dead.is_empty(),
        "neither the pinned child nor its reachable (tombstoned) parent may be reclaimed (VC-3.3)"
    );
}
