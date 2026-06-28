//! VC-6.1 — tail-only branch storage: verification sink (OFFLINE, DETERMINISTIC).
//!
//! Proves two structural properties of the TL-store implementation via the
//! public `BranchStore` API:
//!
//! (a) BYTE-IDENTITY: `BranchStore::load_events(id)` is byte-identical to the
//!     full materialized branch log that `fork` returned. The shared prefix
//!     `[0..fork_tick)` is resolved by reference from the parent and
//!     concatenated with the branch's own tail — reconstructing the exact log
//!     `fork` produced.
//!
//! (b) TAIL-ONLY ON DISK: the `branches/<id>/world-events.jsonl` file holds
//!     ONLY events with `at >= fork_tick`. The shared prefix is NOT cloned to
//!     disk; it is stored once (in the parent) and referenced by pointer via
//!     the `BranchDescriptor`. The on-disk event count equals exactly the tail
//!     length (events at/after the fork tick).
//!
//! NON-VACUITY: the recorded log is designed so that a wrong fork boundary
//! (off-by-one at `fork_tick`, a duplicated boundary event, or a dropped one)
//! would make (a) diverge from the full fork log OR make (b)'s count and
//! per-event `at` check fail. Each event carries a distinct tick and distinct
//! user text so any shift in the boundary produces a visible mismatch in both
//! the byte-identity check and the on-disk inspection.
//!
//! Also covers BRANCH-OF-BRANCH (MAIN ← A ← B): `load_events(B)` resolves
//! the prefix through the full parent chain and reconstructs B's full log
//! byte-identically to what `fork(branch_a_events, ...)` returned.
//!
//! This is a pure IO/filesystem verification sink — no model calls, no async,
//! no live network. Tests run in temp dirs (via `tempfile`) and are
//! deterministic.
//!
//! See docs/agent/world/ecs-runtime.md — Tail-only on-disk storage (VC-6.1).

use rubberdux::agent::world::branch::{
    BranchId, BranchStore, Edit, EditKind, fork, persist_branch,
};
use rubberdux::agent::world::event_log::{EventLog, FilesystemEventLog};
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};

// ---------------------------------------------------------------------------
// Log-builder helpers
// ---------------------------------------------------------------------------

fn ev_session(at: u64, seed: u64) -> Event {
    Event {
        origin: Origin::System,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed,
            surface_tools: Vec::new(),
        },
    }
}

fn ev_user(at: u64, text: &str) -> Event {
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

/// Write `events` as the session MAIN log at `<session_dir>/world-events.jsonl`.
/// This is the shared prefix source that first-level branches resolve against.
fn write_main_log(session_dir: &std::path::Path, events: &[Event]) {
    let mut log = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
    for ev in events {
        log.append(ev).expect("write MAIN log event");
    }
}

/// Read the raw on-disk events from `branches/<id>/world-events.jsonl`.
/// These are the bytes actually written by `persist_branch` — NOT the
/// reconstructed full log.
fn read_branch_disk_events(session_dir: &std::path::Path, id: &BranchId) -> Vec<Event> {
    let path = session_dir
        .join("branches")
        .join(&id.0)
        .join("world-events.jsonl");
    FilesystemEventLog::new(path)
        .load()
        .expect("read raw branch disk file")
}

// ---------------------------------------------------------------------------
// VC-6.1 (a) + (b): byte-identical load + tail-only on-disk inspection
// ---------------------------------------------------------------------------

/// VC-6.1: the headline proof.
///
/// Records a 5-event MAIN log (ticks 0–4), forks at tick 3 (replacing the
/// user message at that tick), persists the branch tail-only, then checks:
///
/// (a) BYTE-IDENTITY — `BranchStore::load_events` reconstructs the FULL
///     branch log (prefix [0..3) ++ tail [3,4]) and the result is
///     byte-identical (Vec<Event> equality) to what `fork` returned.
///
/// (b) TAIL-ONLY ON DISK — the raw `branches/<id>/world-events.jsonl` file
///     holds ONLY 2 events (the tail), every event has `at >= 3`, and the
///     count equals the tail length (2).
///
/// NON-VACUITY:
/// - If the boundary were off-by-one LOW (stored from tick 2): the on-disk
///   file would contain 3 events (ticks 2,3,4); count 3 != 2, and tick 2's
///   `at < 3` would fail the per-event assertion. `load_events` would also
///   reconstruct a different slice and diverge from the fork.
/// - If the boundary were off-by-one HIGH (stored from tick 4): the on-disk
///   file would contain 1 event; count 1 != 2. `load_events` would return
///   prefix[0..3) ++ [tick4 only], missing the boundary event, diverging from
///   the fork.
/// - If a boundary event were duplicated in the tail AND in the prefix: the
///   full reconstruction would have a duplicate at `fork_tick`, diverging from
///   the fork. The on-disk count check (count == tail_len) catches the wrong
///   split even before the byte-identity check fires.
#[test]
fn load_equals_full_fork_and_disk_holds_only_tail() {
    // --- recorded MAIN log (5 events, each with a distinct tick + text) ---
    // Distinct texts make any tick-shift in the boundary immediately visible
    // in the byte-identity comparison.
    let main_log = vec![
        ev_session(0, 0xABCD),
        ev_user(1, "user turn at tick 1"),
        ev_user(2, "user turn at tick 2"),
        ev_user(3, "user turn at tick 3"),
        ev_user(4, "user turn at tick 4"),
    ];
    let fork_tick: u64 = 3;

    // Materialize the counterfactual branch: prefix [0..3) unchanged,
    // tick 3 replaced with a different user message, tick 4 re-stamped.
    let edit = Edit {
        at_tick: fork_tick,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "REPLACED at tick 3".into(),
        },
        kind: EditKind::Replace,
    };
    let (branch_id, full_fork_log) =
        fork(&main_log, fork_tick, edit.clone()).expect("fork succeeds");

    // The full fork log must have the same length as main_log (replace, not extend).
    assert_eq!(
        full_fork_log.len(),
        main_log.len(),
        "a Replace fork keeps the event count identical"
    );

    // Write the MAIN log (the parent prefix source) and persist the branch tail-only.
    let dir = tempfile::tempdir().expect("tempdir");
    let session_dir = dir.path();
    write_main_log(session_dir, &main_log);
    persist_branch(
        session_dir,
        &BranchId::MAIN,
        fork_tick,
        &edit,
        &full_fork_log,
        1_700_000_000,
    )
    .expect("persist branch tail-only");

    // -----------------------------------------------------------------------
    // (a) BYTE-IDENTITY: load_events reconstructs the full fork log exactly.
    // -----------------------------------------------------------------------
    let store = BranchStore::new(session_dir);
    let reconstructed = store
        .load_events(&branch_id)
        .expect("load_events reconstructs full log");

    assert_eq!(
        reconstructed, full_fork_log,
        "VC-6.1(a): BranchStore::load_events must be byte-identical to the full \
         materialized fork log (prefix[0..fork_tick) ++ own tail)"
    );

    // Verify the prefix slice matches the original MAIN prefix verbatim.
    let prefix_from_main: Vec<&Event> = main_log.iter().filter(|e| e.at < fork_tick).collect();
    let prefix_from_reconstructed: Vec<&Event> =
        reconstructed.iter().filter(|e| e.at < fork_tick).collect();
    assert_eq!(
        prefix_from_reconstructed, prefix_from_main,
        "VC-6.1(a): the prefix slice in the reconstructed log must be the original \
         MAIN events verbatim (byte-identical carry-through)"
    );

    // Verify the tail slice carries the edit, not the original message.
    let tail_from_reconstructed: Vec<&Event> =
        reconstructed.iter().filter(|e| e.at >= fork_tick).collect();
    assert_eq!(
        tail_from_reconstructed[0].input,
        edit.input,
        "VC-6.1(a): the boundary event (at == fork_tick) must carry the edit input"
    );

    // -----------------------------------------------------------------------
    // (b) TAIL-ONLY ON DISK: the branch file holds ONLY events >= fork_tick.
    // -----------------------------------------------------------------------
    let disk_events = read_branch_disk_events(session_dir, &branch_id);

    // The expected tail is all events from the fork result at/after fork_tick.
    let expected_tail: Vec<Event> = full_fork_log
        .iter()
        .filter(|e| e.at >= fork_tick)
        .cloned()
        .collect();

    // Count equality: the on-disk file holds exactly as many events as the tail.
    // An off-by-one boundary means count != expected_tail.len().
    assert_eq!(
        disk_events.len(),
        expected_tail.len(),
        "VC-6.1(b): on-disk event count must equal the tail length (events >= fork_tick); \
         a wrong boundary produces a wrong count"
    );

    // Per-event at-check: every on-disk event must be >= fork_tick.
    // An off-by-one-LOW boundary would include a prefix event with at < fork_tick.
    for ev in &disk_events {
        assert!(
            ev.at >= fork_tick,
            "VC-6.1(b): on-disk event at tick {} must satisfy at >= fork_tick ({}); \
             the prefix must NOT be cloned to the branch file",
            ev.at,
            fork_tick
        );
    }

    // Content equality: the on-disk events match the fork's tail exactly.
    assert_eq!(
        disk_events, expected_tail,
        "VC-6.1(b): the on-disk branch file must hold exactly the tail (fork result \
         events at/after fork_tick), not a prefix clone"
    );

    // Structural proof that the prefix is ABSENT from the on-disk file:
    // none of the prefix events (at < fork_tick) appear on disk.
    for prefix_ev in main_log.iter().filter(|e| e.at < fork_tick) {
        assert!(
            !disk_events.contains(prefix_ev),
            "VC-6.1(b): prefix event at tick {} must NOT appear in the on-disk \
             branch file (prefix is shared by pointer, never duplicated)",
            prefix_ev.at
        );
    }
}

// ---------------------------------------------------------------------------
// VC-6.1 — off-by-one boundary non-vacuity: explicit boundary-event check
// ---------------------------------------------------------------------------

/// NON-VACUITY: prove the fork_tick boundary event is IN the tail (on disk)
/// and NOT in the prefix (off-disk). This is the exact off-by-one boundary
/// that a wrong implementation (fork_tick stored as fork_tick+1 or fork_tick-1)
/// would get wrong. The boundary event is the FIRST tail event; it must be on
/// disk and must carry the edit input.
#[test]
fn boundary_event_is_in_tail_not_prefix() {
    let main_log = vec![
        ev_session(0, 0x1234),
        ev_user(1, "before fork"),
        ev_user(2, "at fork boundary"),
        ev_user(3, "after fork"),
    ];
    let fork_tick: u64 = 2; // tick 2 is the boundary event

    let edit = Edit {
        at_tick: fork_tick,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "BOUNDARY REPLACED".into(),
        },
        kind: EditKind::Replace,
    };
    let (branch_id, full_fork_log) =
        fork(&main_log, fork_tick, edit.clone()).expect("fork succeeds");

    let dir = tempfile::tempdir().expect("tempdir");
    let session_dir = dir.path();
    write_main_log(session_dir, &main_log);
    persist_branch(
        session_dir,
        &BranchId::MAIN,
        fork_tick,
        &edit,
        &full_fork_log,
        0,
    )
    .expect("persist");

    let disk_events = read_branch_disk_events(session_dir, &branch_id);

    // The boundary event (at == fork_tick) MUST be in the tail (on disk).
    let boundary_on_disk = disk_events.iter().find(|e| e.at == fork_tick);
    assert!(
        boundary_on_disk.is_some(),
        "VC-6.1 boundary: the event at fork_tick ({}) must be in the on-disk \
         tail — an off-by-one HIGH would exclude it",
        fork_tick
    );
    assert_eq!(
        boundary_on_disk.unwrap().input,
        edit.input,
        "VC-6.1 boundary: the boundary event on disk must carry the edit input"
    );

    // The event BEFORE the boundary (at == fork_tick - 1) must NOT be on disk.
    let pre_boundary_on_disk = disk_events.iter().find(|e| e.at == fork_tick - 1);
    assert!(
        pre_boundary_on_disk.is_none(),
        "VC-6.1 boundary: the event at fork_tick-1 ({}) must NOT be in the on-disk \
         tail — an off-by-one LOW would include it (prefix duplication)",
        fork_tick - 1
    );

    // Full reconstruction is still byte-identical.
    let store = BranchStore::new(session_dir);
    let reconstructed = store.load_events(&branch_id).expect("load_events");
    assert_eq!(
        reconstructed, full_fork_log,
        "VC-6.1 boundary: byte-identical reconstruction holds at the exact boundary"
    );
}

// ---------------------------------------------------------------------------
// VC-6.1 — BRANCH-OF-BRANCH (MAIN ← A ← B): recursive chain reconstruction
// ---------------------------------------------------------------------------

/// VC-6.1 (branch-of-branch): a three-level chain `MAIN ← A ← B` reconstructs
/// B's full log byte-identically through the entire ancestor chain.
///
/// Layout:
///   MAIN: ticks 0..5 (6 events)
///   A = fork MAIN at tick 2 (prefix [0,1], tail [2..5])  →  parent = MAIN
///   B = fork A's full log at tick 4 (prefix [0,1,2,3], tail [4,5])  →  parent = A
///
/// Proof:
/// (a) `load_events(B)` == `fork(branch_a_events, 4, edit_b)` byte-identically.
/// (b) B's on-disk file holds ONLY events with `at >= 4` (2 events).
/// (c) A's on-disk file holds ONLY events with `at >= 2` (4 events).
///
/// The on-disk inspection of A proves that even intermediate levels are
/// tail-only — neither A nor B duplicates the MAIN prefix.
#[test]
fn branch_of_branch_reconstructs_byte_identically_through_chain() {
    // MAIN log: 6 events at ticks 0–5. Distinct texts ensure any chain-level
    // boundary error is visible in the byte-identity comparison.
    let main_log = vec![
        ev_session(0, 0xDEAD),
        ev_user(1, "MAIN turn 1"),
        ev_user(2, "MAIN turn 2"),
        ev_user(3, "MAIN turn 3"),
        ev_user(4, "MAIN turn 4"),
        ev_user(5, "MAIN turn 5"),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let session_dir = dir.path();
    write_main_log(session_dir, &main_log);

    // ---- A: fork MAIN at tick 2 ----------------------------------------
    let fork_tick_a: u64 = 2;
    let edit_a = Edit {
        at_tick: fork_tick_a,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "A edit at tick 2".into(),
        },
        kind: EditKind::Replace,
    };
    let (id_a, branch_a_events) =
        fork(&main_log, fork_tick_a, edit_a.clone()).expect("fork A from MAIN");

    persist_branch(
        session_dir,
        &BranchId::MAIN,
        fork_tick_a,
        &edit_a,
        &branch_a_events,
        1_000,
    )
    .expect("persist A");

    // A's on-disk file must hold ONLY events with `at >= 2`.
    let disk_a = read_branch_disk_events(session_dir, &id_a);
    let expected_tail_a: Vec<Event> = branch_a_events
        .iter()
        .filter(|e| e.at >= fork_tick_a)
        .cloned()
        .collect();

    assert_eq!(
        disk_a.len(),
        expected_tail_a.len(),
        "VC-6.1 chain(A): on-disk count for A must equal A's tail length"
    );
    for ev in &disk_a {
        assert!(
            ev.at >= fork_tick_a,
            "VC-6.1 chain(A): all A's on-disk events must have at >= {} (got {})",
            fork_tick_a,
            ev.at
        );
    }

    // ---- B: fork A's full log at tick 4 --------------------------------
    let fork_tick_b: u64 = 4;
    let edit_b = Edit {
        at_tick: fork_tick_b,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "B edit at tick 4".into(),
        },
        kind: EditKind::Replace,
    };
    // B is materialized from A's FULL materialized log (branch_a_events).
    let (_id_b_off_main, branch_b_events) =
        fork(&branch_a_events, fork_tick_b, edit_b.clone()).expect("fork B from A");

    // Persist B with parent = id_a (not MAIN).
    let desc_b = persist_branch(
        session_dir,
        &id_a,
        fork_tick_b,
        &edit_b,
        &branch_b_events,
        2_000,
    )
    .expect("persist B");

    // ---- (a) BYTE-IDENTITY: load_events(B) == full fork of A at tick 4 ---
    let store = BranchStore::new(session_dir);
    let reconstructed_b = store
        .load_events(&desc_b.id)
        .expect("load_events for B through the parent chain");

    assert_eq!(
        reconstructed_b, branch_b_events,
        "VC-6.1 chain(b): BranchStore::load_events(B) must be byte-identical to \
         the full materialized fork(branch_a, 4, edit_b) — the chain \
         MAIN ← A ← B reconstructs correctly"
    );

    // Sanity: B's prefix [0..4) equals A's full log's first 4 events.
    let b_prefix_from_reconstructed: Vec<&Event> =
        reconstructed_b.iter().filter(|e| e.at < fork_tick_b).collect();
    let b_prefix_from_a: Vec<&Event> =
        branch_a_events.iter().filter(|e| e.at < fork_tick_b).collect();
    assert_eq!(
        b_prefix_from_reconstructed, b_prefix_from_a,
        "VC-6.1 chain: B's reconstructed prefix [0..fork_tick_b) must equal A's \
         full log events [0..fork_tick_b) byte-for-byte"
    );

    // ---- (b) TAIL-ONLY ON DISK for B -----------------------------------
    let disk_b = read_branch_disk_events(session_dir, &desc_b.id);
    let expected_tail_b: Vec<Event> = branch_b_events
        .iter()
        .filter(|e| e.at >= fork_tick_b)
        .cloned()
        .collect();

    assert_eq!(
        disk_b.len(),
        expected_tail_b.len(),
        "VC-6.1 chain(B): on-disk count for B must equal B's tail length"
    );
    for ev in &disk_b {
        assert!(
            ev.at >= fork_tick_b,
            "VC-6.1 chain(B): all B's on-disk events must have at >= {} (got {})",
            fork_tick_b,
            ev.at
        );
    }
    assert_eq!(
        disk_b, expected_tail_b,
        "VC-6.1 chain(B): B's on-disk events must equal the fork's tail exactly"
    );

    // Structural proof: MAIN's prefix events (at < fork_tick_a) are absent
    // from BOTH A's and B's on-disk files — the prefix is shared, never cloned.
    for prefix_ev in main_log.iter().filter(|e| e.at < fork_tick_a) {
        assert!(
            !disk_a.contains(prefix_ev),
            "VC-6.1 chain: MAIN prefix event at tick {} must not appear in A's \
             on-disk file",
            prefix_ev.at
        );
        assert!(
            !disk_b.contains(prefix_ev),
            "VC-6.1 chain: MAIN prefix event at tick {} must not appear in B's \
             on-disk file",
            prefix_ev.at
        );
    }
}

// ---------------------------------------------------------------------------
// VC-6.1 — multiple branches off the same MAIN: independent tail-only files
// ---------------------------------------------------------------------------

/// VC-6.1 (multi-branch): two branches forked from the SAME MAIN log at
/// different ticks each persist their own tail-only file independently. Neither
/// branch's on-disk file contains the other's tail, and both reconstruct their
/// full fork byte-identically. This proves tail-only storage is per-branch, not
/// shared or merged.
#[test]
fn multiple_branches_off_main_each_persist_independent_tails() {
    let main_log = vec![
        ev_session(0, 0xCAFE),
        ev_user(1, "shared turn 1"),
        ev_user(2, "shared turn 2"),
        ev_user(3, "diverge point A"),
        ev_user(4, "diverge point B"),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let session_dir = dir.path();
    write_main_log(session_dir, &main_log);

    // Branch-1: fork at tick 2 (prefix [0,1], tail [2,3,4]).
    let edit_1 = Edit {
        at_tick: 2,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "branch-1 edit".into(),
        },
        kind: EditKind::Replace,
    };
    let (id_1, fork_1) = fork(&main_log, 2, edit_1.clone()).expect("fork branch-1");
    persist_branch(session_dir, &BranchId::MAIN, 2, &edit_1, &fork_1, 0)
        .expect("persist branch-1");

    // Branch-2: fork at tick 4 (prefix [0,1,2,3], tail [4]).
    let edit_2 = Edit {
        at_tick: 4,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "branch-2 edit".into(),
        },
        kind: EditKind::Replace,
    };
    let (id_2, fork_2) = fork(&main_log, 4, edit_2.clone()).expect("fork branch-2");
    persist_branch(session_dir, &BranchId::MAIN, 4, &edit_2, &fork_2, 0)
        .expect("persist branch-2");

    let store = BranchStore::new(session_dir);

    // Branch-1 reconstructs byte-identically and has 3 on-disk events (ticks 2,3,4).
    let recon_1 = store.load_events(&id_1).expect("load branch-1");
    assert_eq!(
        recon_1, fork_1,
        "VC-6.1 multi: branch-1 reconstructs byte-identically"
    );
    let disk_1 = read_branch_disk_events(session_dir, &id_1);
    assert_eq!(disk_1.len(), 3, "branch-1 tail has 3 events (ticks 2,3,4)");
    assert!(disk_1.iter().all(|e| e.at >= 2), "branch-1 disk: all at >= 2");

    // Branch-2 reconstructs byte-identically and has 1 on-disk event (tick 4).
    let recon_2 = store.load_events(&id_2).expect("load branch-2");
    assert_eq!(
        recon_2, fork_2,
        "VC-6.1 multi: branch-2 reconstructs byte-identically"
    );
    let disk_2 = read_branch_disk_events(session_dir, &id_2);
    assert_eq!(disk_2.len(), 1, "branch-2 tail has 1 event (tick 4)");
    assert!(disk_2.iter().all(|e| e.at >= 4), "branch-2 disk: all at >= 4");

    // The two branch files are independent — no cross-contamination.
    for ev in &disk_1 {
        assert!(
            !disk_2.contains(ev),
            "branch-1's tail event at {} must not appear in branch-2's on-disk file",
            ev.at
        );
    }
}
