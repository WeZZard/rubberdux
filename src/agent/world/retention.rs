// Consumed by CR-cli-prune and SnapshotStore::prune; silence dead-code lints
// until the CLI caller lands (CR-cli-prune).
#![allow(dead_code)]

//! Snapshot retention policy: keep only the latest K snapshots, evicting
//! older ones. Evicted snapshots are regenerable by replay from an earlier
//! snapshot (or genesis) plus the event-log tail — they hold no unique data.
//!
//! The module mirrors the tick/drive_live split used throughout the ECS
//! runtime: `evictable_snapshots` is a PURE policy function (no I/O, fully
//! unit-testable in isolation) and `reclaim` is the thin IO shell that
//! executes the returned plan.
//!
//! See docs/agent/world/ecs-runtime.md — PRUNE / snapshot retention
//! (Inv 9/11).

use crate::agent::world::snapshot::SnapshotStore;
use crate::agent::world::world::Tick;
use crate::error::Error;

// ---------------------------------------------------------------------------
// Pure policy
// ---------------------------------------------------------------------------

/// Compute the set of snapshot ticks that are safe to evict under a keep-K
/// retention policy.
///
/// # Arguments
///
/// * `snapshot_ticks` — every tick for which a snapshot file currently exists.
/// * `keep` — the number of most-recent snapshots to protect. `0` means
///   unbounded retention (no snapshot is ever evictable).
/// * `live_tail` — the current live frontier of the session (the highest
///   recorded tick). Snapshots with a tick greater than `live_tail` are
///   always protected (the session hasn't advanced past them in the hot log).
///
/// # Returns
///
/// A `Vec<Tick>` naming every snapshot that may be deleted. Two invariants
/// hold regardless of the input:
///
/// * **Inv 9/11** — every evicted tick is regenerable: the caller retains
///   at least one earlier snapshot (or genesis) plus the full event-log tail.
/// * `keep == 0` ⇒ empty return (unbounded; no snapshot is removed).
/// * Ticks beyond `live_tail` are never returned (they are not yet fully
///   covered by the hot log and cannot be reclaimed safely).
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / evictable_snapshots.
pub fn evictable_snapshots(snapshot_ticks: &[Tick], keep: u32, live_tail: Tick) -> Vec<Tick> {
    if keep == 0 {
        // Unbounded retention: evict nothing.
        return Vec::new();
    }

    // Only ticks at or before the live frontier are candidates for eviction.
    // A tick beyond live_tail hasn't been replayed past yet; always protect it.
    let mut candidates: Vec<Tick> = snapshot_ticks
        .iter()
        .copied()
        .filter(|&t| t <= live_tail)
        .collect();

    // Sort descending so the most-recent (highest-tick) snapshots come first,
    // then deduplicate by value. Deduplication must happen AFTER sorting so
    // that a tick value which appears multiple times in the input (e.g. from a
    // filesystem scan that returned the same file twice) cannot straddle the
    // keep boundary: without it one copy would be "protected" and another
    // "evictable", yet both map to the same `<tick>.snapshot.json` file —
    // deleting the "evictable" copy would destroy the protected snapshot.
    // After dedup the protected set and the evictable set are guaranteed
    // disjoint by value.
    candidates.sort_unstable_by(|a, b| b.cmp(a));
    candidates.dedup();

    // The first `keep` entries in descending order are the LATEST K — they
    // are protected. Everything after that is evictable.
    if candidates.len() <= keep as usize {
        return Vec::new();
    }

    // Skip the protected head; the tail is the evictable set.
    candidates.into_iter().skip(keep as usize).collect()
}

// ---------------------------------------------------------------------------
// Thin IO shell
// ---------------------------------------------------------------------------

/// Delete all evictable snapshots from `store` under a keep-K policy,
/// returning the ticks that were successfully removed.
///
/// This is the thin IO shell over the pure `evictable_snapshots` policy:
///
/// 1. List every snapshot tick in `store`.
/// 2. Compute the evictable set via `evictable_snapshots`.
/// 3. Delete each evictable snapshot file via `store.remove(tick)`.
///
/// A deletion failure propagates immediately. A partial reclaim (where some
/// files were removed before the error) is safe: any deleted snapshot is
/// regenerable from an earlier snapshot/genesis plus the event-log tail
/// (Inv 9/11). No event-log file is ever touched here.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / reclaim shell.
pub fn reclaim(store: &SnapshotStore, keep: u32, live_tail: Tick) -> Result<Vec<Tick>, Error> {
    let all_ticks = store.list_ticks()?;
    let to_evict = evictable_snapshots(&all_ticks, keep, live_tail);
    for &tick in &to_evict {
        store.remove(tick)?;
    }
    Ok(to_evict)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::agent::world::budget::Budget;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::History;
    use crate::agent::world::inputs::{Event, LogicalInput, Origin};
    use crate::agent::world::replay::fold_log;
    use crate::agent::world::snapshot::{capture, snapshot_at, SnapshotStore};
    use crate::agent::world::world::{
        Activity, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources,
    };

    const SEED: u64 = 7;

    fn offline_model() -> ModelConfig {
        ModelConfig {
            model: "claude-retention-unit".into(),
            max_tokens: 256,
            effort: Effort::Low,
        }
    }

    /// Minimal genesis consistent with the canonical test genesis in replay.rs.
    fn make_genesis(seed: u64) -> crate::agent::world::world::World {
        use crate::agent::world::world::World;
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

    fn session_started_event() -> Event {
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

    fn user_message_event(at: u64, text: &str) -> Event {
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

    // -----------------------------------------------------------------------
    // Pure policy tests (no I/O)
    // -----------------------------------------------------------------------

    /// With `keep = K`, the latest K snapshots by tick are protected and the
    /// rest are returned as evictable.
    #[test]
    fn keeps_latest_k_snapshots() {
        let ticks: Vec<Tick> = vec![1, 2, 3, 4, 5];
        let live_tail: Tick = 5;

        let evictable = evictable_snapshots(&ticks, 2, live_tail);

        // Latest 2 by tick are 5 and 4 — protected. Ticks 1, 2, 3 are evictable.
        let mut evictable_sorted = evictable.clone();
        evictable_sorted.sort_unstable();
        assert_eq!(
            evictable_sorted,
            vec![1, 2, 3],
            "ticks 1, 2, 3 must be evictable when keep=2 and live_tail=5"
        );
    }

    /// With `keep = 0` (unbounded), no snapshot is ever evictable.
    #[test]
    fn keep_zero_means_unbounded() {
        let ticks: Vec<Tick> = vec![1, 2, 3, 100, 200];
        let evictable = evictable_snapshots(&ticks, 0, 1000);
        assert!(
            evictable.is_empty(),
            "keep=0 must produce an empty evictable set; got {evictable:?}"
        );
    }

    /// When the total number of snapshots does not exceed `keep`, no snapshot
    /// is evictable (the evictable set is empty).
    #[test]
    fn fewer_than_k_snapshots_evicts_nothing() {
        let ticks: Vec<Tick> = vec![1, 2];
        let evictable = evictable_snapshots(&ticks, 5, 10);
        assert!(
            evictable.is_empty(),
            "fewer snapshots than keep must yield empty evictable set; got {evictable:?}"
        );
    }

    /// The evictable set is exactly the ticks NOT in the latest K set. The
    /// union of the evictable set and the protected set is the full input.
    #[test]
    fn evictable_set_is_complement_of_latest_k() {
        let ticks: Vec<Tick> = vec![10, 20, 30, 40, 50];
        let keep: u32 = 3;
        let live_tail: Tick = 50;

        let evictable = evictable_snapshots(&ticks, keep, live_tail);

        // Latest 3 by tick: 50, 40, 30 — protected. 10 and 20 are evictable.
        let mut evictable_sorted = evictable.clone();
        evictable_sorted.sort_unstable();
        assert_eq!(
            evictable_sorted,
            vec![10, 20],
            "evictable set must be the complement of the latest-K protected set"
        );

        // Verify: union of evictable and protected equals the full input.
        let protected: Vec<Tick> = ticks.iter().copied().filter(|t| !evictable.contains(t)).collect();
        let mut all_reconstructed: Vec<Tick> = evictable.iter().copied().chain(protected).collect();
        all_reconstructed.sort_unstable();
        let mut original_sorted = ticks.clone();
        original_sorted.sort_unstable();
        assert_eq!(
            all_reconstructed, original_sorted,
            "evictable ∪ protected must equal the full input tick set"
        );
    }

    /// Ticks beyond `live_tail` are NEVER included in the evictable set, even
    /// when they would otherwise exceed the keep window.
    #[test]
    fn ticks_beyond_live_tail_are_protected() {
        // Snapshots at 1, 2, 3; live tail is only at 2, so tick 3 is beyond it.
        let ticks: Vec<Tick> = vec![1, 2, 3];
        let evictable = evictable_snapshots(&ticks, 1, 2);

        // Only ticks ≤ 2 are candidates: [1, 2]. Keep latest 1 ⇒ protect 2.
        // Tick 3 must never appear in the evictable set.
        assert!(
            !evictable.contains(&3),
            "tick 3 is beyond live_tail=2 and must never be evictable"
        );
        // Tick 1 is the sole evictable candidate.
        assert!(
            evictable.contains(&1),
            "tick 1 must be evictable (only 1 kept and tick 2 > tick 1)"
        );
    }

    // -----------------------------------------------------------------------
    // Duplicate-tick / input-order robustness tests (pure policy)
    // -----------------------------------------------------------------------

    /// Duplicate tick values in the input are collapsed before the keep-K
    /// boundary is applied, so no tick value can appear in BOTH the protected
    /// set and the evictable set.  The returned ticks must be distinct.
    #[test]
    fn duplicate_ticks_never_split_across_keep_boundary() {
        // Tick 2 appears twice — a filesystem scan that returned the same file
        // twice, for example.
        let ticks: Vec<Tick> = vec![1, 2, 2, 3];
        let live_tail: Tick = 3;

        let evictable = evictable_snapshots(&ticks, 1, live_tail);

        // Latest 1 by distinct value is tick 3 — protected.
        // Ticks 1 and 2 are evictable (each at most once).
        let mut evictable_sorted = evictable.clone();
        evictable_sorted.sort_unstable();
        assert_eq!(
            evictable_sorted,
            vec![1, 2],
            "duplicate input tick 2 must collapse to one evictable entry"
        );

        // Output must be distinct — no tick value repeated.
        let unique_count = {
            let mut v = evictable.clone();
            v.sort_unstable();
            v.dedup();
            v.len()
        };
        assert_eq!(
            unique_count,
            evictable.len(),
            "evictable output must contain only distinct tick values"
        );

        // Disjoint check: no value in both protected and evictable.
        let evictable_set: std::collections::HashSet<Tick> =
            evictable.iter().copied().collect();
        // All distinct candidates ≤ live_tail.
        let all_distinct: std::collections::HashSet<Tick> =
            vec![1u64, 2, 3].into_iter().collect();
        let protected: std::collections::HashSet<Tick> = all_distinct
            .difference(&evictable_set)
            .copied()
            .collect();
        for t in &evictable {
            assert!(
                !protected.contains(t),
                "tick {t} must not appear in both protected and evictable sets"
            );
        }
    }

    /// With all-duplicate input (every tick is the same value) the
    /// function returns an empty evictable set because `keep ≥ 1` protects
    /// the one distinct tick.
    #[test]
    fn all_duplicate_ticks_evicts_nothing() {
        let ticks: Vec<Tick> = vec![5, 5, 5, 5];
        let evictable = evictable_snapshots(&ticks, 1, 5);
        assert!(
            evictable.is_empty(),
            "all-duplicate input with keep=1 must evict nothing; got {evictable:?}"
        );
    }

    /// When the K-th and (K+1)-th candidates have the same tick value (a tie
    /// at the keep boundary after dedup), that value is protected — the dedup
    /// collapses it to one entry, and the K-latest-by-value rule keeps it.
    #[test]
    fn k_boundary_tie_protects_the_tied_tick() {
        // After dedup the distinct sorted-desc candidates are [5, 3, 1].
        // keep=2 protects 5 and 3; only tick 1 is evictable.
        let ticks: Vec<Tick> = vec![5, 5, 3, 1];
        let live_tail: Tick = 5;

        let evictable = evictable_snapshots(&ticks, 2, live_tail);

        let mut evictable_sorted = evictable.clone();
        evictable_sorted.sort_unstable();
        assert_eq!(
            evictable_sorted,
            vec![1],
            "only tick 1 is evictable; the tied tick 5 must remain protected"
        );
    }

    /// Unsorted input: the result must match the result for the same ticks
    /// presented in sorted order, proving that input order does not affect
    /// which ticks are protected.
    #[test]
    fn unsorted_input_matches_sorted_result() {
        let sorted_ticks: Vec<Tick> = vec![1, 2, 3, 4, 5];
        let unsorted_ticks: Vec<Tick> = vec![3, 1, 5, 2, 4];
        let live_tail: Tick = 5;
        let keep: u32 = 2;

        let mut evict_sorted = evictable_snapshots(&sorted_ticks, keep, live_tail);
        let mut evict_unsorted = evictable_snapshots(&unsorted_ticks, keep, live_tail);
        evict_sorted.sort_unstable();
        evict_unsorted.sort_unstable();

        assert_eq!(
            evict_sorted, evict_unsorted,
            "evictable set must be identical regardless of input order"
        );
        assert_eq!(
            evict_sorted,
            vec![1, 2, 3],
            "latest 2 are ticks 5 and 4; ticks 1, 2, 3 must be evictable"
        );
    }

    /// Reverse-sorted input: same as unsorted, confirming descending input
    /// produces the correct latest-K result.
    #[test]
    fn reverse_sorted_input_gives_correct_latest_k() {
        let ticks: Vec<Tick> = vec![5, 4, 3, 2, 1];
        let live_tail: Tick = 5;

        let evictable = evictable_snapshots(&ticks, 2, live_tail);

        let mut evictable_sorted = evictable.clone();
        evictable_sorted.sort_unstable();
        assert_eq!(
            evictable_sorted,
            vec![1, 2, 3],
            "reverse-sorted input: ticks 1, 2, 3 must be evictable when keep=2 and live_tail=5"
        );
    }

    // -----------------------------------------------------------------------
    // IO test: reclaim + regenerate round-trip
    // -----------------------------------------------------------------------

    /// Reclaim + regenerate round-trip (VC-3.1):
    /// - Write snapshots at several ticks.
    /// - Call `reclaim` to keep only the latest 1.
    /// - The evicted snapshot files must be gone from disk.
    /// - The World at any evicted tick is still regenerable by replaying the
    ///   event log from genesis, proving that deleting the snapshot loses no
    ///   unique data (Inv 9/11).
    #[test]
    fn reclaim_and_regenerate_round_trip() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));

        // Build a small log: SessionStarted at tick 0, then 3 user messages.
        let events = vec![
            session_started_event(),
            user_message_event(1, "alpha"),
            user_message_event(2, "beta"),
            user_message_event(3, "gamma"),
        ];

        // Capture and write snapshots at ticks 1, 2, and 3.
        let snap1 = snapshot_at(&events, make_genesis, 1).expect("snap at 1");
        let snap2 = snapshot_at(&events, make_genesis, 2).expect("snap at 2");
        let snap3 = snapshot_at(&events, make_genesis, 3).expect("snap at 3");
        store.write(&snap1).expect("write snap1");
        store.write(&snap2).expect("write snap2");
        store.write(&snap3).expect("write snap3");

        // Reclaim keeping only the latest 1 snapshot; live tail is at tick 3.
        let evicted = reclaim(&store, 1, 3).expect("reclaim must succeed");

        // Ticks 1 and 2 must have been evicted (latest 1 is tick 3).
        let mut evicted_sorted = evicted.clone();
        evicted_sorted.sort_unstable();
        assert_eq!(
            evicted_sorted,
            vec![1, 2],
            "ticks 1 and 2 must be evicted when keep=1 and live_tail=3"
        );

        // The evicted snapshot files must no longer exist on disk.
        let snapshots_dir = dir.path().join("snapshots");
        assert!(
            !snapshots_dir.join("1.snapshot.json").exists(),
            "tick-1 snapshot file must be removed by reclaim"
        );
        assert!(
            !snapshots_dir.join("2.snapshot.json").exists(),
            "tick-2 snapshot file must be removed by reclaim"
        );

        // The retained snapshot (tick 3) must still exist.
        assert!(
            snapshots_dir.join("3.snapshot.json").exists(),
            "tick-3 snapshot file must be retained"
        );

        // Regenerability proof: replay from genesis using the original event
        // log to reconstruct the World at tick 1 (an evicted tick).
        let genesis = make_genesis(SEED);
        let prefix_t1: Vec<Event> = events.iter().filter(|e| e.at <= 1).cloned().collect();
        let replayed_world_t1 = fold_log(genesis.clone(), &prefix_t1);

        // The replayed world must match what the now-deleted snapshot had.
        assert_eq!(
            serde_json::to_vec(&replayed_world_t1).expect("serialize replayed"),
            serde_json::to_vec(&snap1.world).expect("serialize original snap1.world"),
            "the World at the evicted tick must be regenerable byte-identically from the log"
        );

        // Same proof for tick 2.
        let prefix_t2: Vec<Event> = events.iter().filter(|e| e.at <= 2).cloned().collect();
        let replayed_world_t2 = fold_log(genesis, &prefix_t2);
        assert_eq!(
            serde_json::to_vec(&replayed_world_t2).expect("serialize replayed t2"),
            serde_json::to_vec(&snap2.world).expect("serialize original snap2.world"),
            "the World at evicted tick 2 must be regenerable byte-identically from the log"
        );
    }
}
