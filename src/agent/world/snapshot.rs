// These items are consumed by the snapshot store (CR-snapshot-store) and
// restore (CR-restore) tasks; the binary target has no caller yet, so silence
// dead-code lints here.
#![allow(dead_code)]

//! Snapshot — a full-World serialization carrying a content digest, and
//! `SnapshotStore` — the filesystem-backed writer/loader for those snapshots.
//!
//! `Snapshot` is a pure value type: no filesystem I/O, no side effects.
//! `SnapshotStore` owns a snapshots directory and provides atomic writes
//! (`write`), a fallback-capable nearest-≤ loader (`nearest_at_or_before`),
//! and a retention seam (`prune`).
//!
//! See docs/agent/world/ecs-runtime.md — Restore snapshots.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::world::inputs::{Event, Fingerprint};
use crate::agent::world::replay::{fold_log, genesis_from_log};
use crate::agent::world::world::{Tick, World};
use crate::error::Error;

// ---------------------------------------------------------------------------
// Snapshot — the full-World value type
// ---------------------------------------------------------------------------

/// A full-World snapshot at a given logical tick, carrying a SHA-256 content
/// digest over the canonical serialization of `world`.
///
/// The digest is `hex(SHA-256(serde_json::to_vec(&world)))`, identical to the
/// fingerprint scheme used by `effects::fingerprint_call` (Inv 7). Two
/// snapshots with equal digests carry byte-identical worlds; structural
/// equality of `world` follows from digest equality via the canonicalization
/// that `serde_json::to_vec` + `BTreeMap` iteration order guarantee.
///
/// See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), Inv 7
/// (content-addressed replay — the fingerprint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The logical tick (`World::clock`) at the moment of capture.
    pub tick: Tick,
    /// `hex(SHA-256(serde_json::to_vec(&world)))` — the canonical byte digest.
    pub digest: Fingerprint,
    /// The serializable World state at `tick`.
    pub world: World,
}

impl Snapshot {
    /// Verify integrity: recompute the digest from `self.world` and compare it
    /// to `self.digest`. Returns `true` for an honest, untampered snapshot and
    /// `false` if any field was altered after capture.
    pub fn validate(&self) -> bool {
        match world_digest(&self.world) {
            Ok(d) => d == self.digest,
            Err(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// digest helper (pure, no I/O)
// ---------------------------------------------------------------------------

/// Compute `hex(SHA-256(serde_json::to_vec(&world)))`.
///
/// `World` derives `Serialize` with only integer/string/enum fields inside
/// `BTreeMap`s (no floats, no cycles) — the same constraints the byte-identity
/// replay tests rely on — so `serde_json::to_vec` is infallible in practice.
/// The `?` operator propagates the (theoretical) `serde_json::Error` so callers
/// need not unwrap it.
fn world_digest(world: &World) -> Result<Fingerprint, Error> {
    let bytes = serde_json::to_vec(world)?;
    Ok(Fingerprint(hex::encode(Sha256::digest(&bytes))))
}

// ---------------------------------------------------------------------------
// Public API — pure capture / validate / snapshot_at
// ---------------------------------------------------------------------------

/// Capture a snapshot of `world` at its current tick.
///
/// Computes `digest = hex(SHA-256(serde_json::to_vec(&world)))` using the same
/// canonicalization that makes replay byte-identical (Inv 6/7). The world is
/// moved into the snapshot; no copy of the state is retained by the caller.
///
/// See docs/agent/world/ecs-runtime.md — snapshots.
pub fn capture(world: World) -> Snapshot {
    // SAFETY NOTE: `World` contains only integer, string, enum, BTreeMap, and
    // Vec fields — no floats and no cycles.  `serde_json::to_vec` on such a
    // type is infallible.  The expect is load-bearing documentation: if a future
    // contributor adds a float field without noticing, this surfaces the
    // invariant violation immediately rather than silently corrupting the digest.
    let digest = world_digest(&world)
        .expect("World serialization must not fail: it contains no floats or cycles");
    Snapshot {
        tick: world.clock,
        digest,
        world,
    }
}

/// Reconstruct the `World` at `tick` from a recorded event log and capture a
/// snapshot. Events with `e.at > tick` are excluded so the result is the world
/// the log converges to AT that tick.
///
/// The resulting `snapshot.world` is byte-identical to
/// `fold_log(genesis, events[..=tick])`, which is the canonical live world the
/// replay spine guarantees (Inv 6).
///
/// `build_genesis` is called with the seed extracted from the log's
/// `SessionStarted` header (the one hidden input that must cross a recorded
/// boundary, Inv 8); the caller supplies the SAME genesis constructor the live
/// run used. If the log contains no `SessionStarted` header this returns
/// `Error::World`.
///
/// See docs/agent/world/ecs-runtime.md — Inv 8 (recorded seed), Inv 9
/// (log is single source of truth).
pub fn snapshot_at(
    events: &[Event],
    build_genesis: impl FnOnce(u64) -> World,
    tick: Tick,
) -> Result<Snapshot, Error> {
    let prefix: Vec<Event> = events
        .iter()
        .filter(|e| e.at <= tick)
        .cloned()
        .collect();
    let genesis = genesis_from_log(&prefix, build_genesis)?;
    let world = fold_log(genesis, &prefix);
    Ok(capture(world))
}

// ---------------------------------------------------------------------------
// SnapshotStore — filesystem-backed writer + nearest-≤ loader
// ---------------------------------------------------------------------------

/// A directory-scoped store for tick-indexed snapshot files.
///
/// Each snapshot is persisted as `<dir>/<tick>.snapshot.json`. Writes are
/// atomic: the file is first written to a sibling `.tmp` path in the SAME
/// directory (guaranteeing the same filesystem for the subsequent rename),
/// fsynced, then renamed into place, so the log (the true source of truth)
/// is never harmed by a partial write (Inv 9).
///
/// `nearest_at_or_before` validates every candidate with `Snapshot::validate`
/// (digest recompute) before returning it, falling back through earlier
/// snapshots until one passes or `None` is returned — signalling that the
/// caller should fall back to genesis (Inv 4 / VC-2.3).
///
/// Rejected (corrupt) snapshots are NEVER deleted by the loader — the log
/// alone is the source of truth and can always regenerate a correct snapshot.
///
/// See docs/agent/world/ecs-runtime.md — Restore snapshots.
pub struct SnapshotStore {
    dir: PathBuf,
}

impl SnapshotStore {
    /// Create a `SnapshotStore` rooted at `dir`.
    ///
    /// The directory and any missing parents are created on the first `write`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Write `snapshot` to `<dir>/<tick>.snapshot.json` atomically and
    /// non-destructively.
    ///
    /// Before publishing, any file already at the final path is inspected:
    ///
    /// * **Identical valid snapshot** — same digest as the incoming snapshot:
    ///   the write is treated as an idempotent no-op and `Ok(())` is returned
    ///   immediately; no temp file is created.
    /// * **Different valid snapshot** — same tick but a distinct digest: a
    ///   deterministic snapshot store must never produce two differing valid
    ///   snapshots for the same tick, so `Err` is returned and the existing
    ///   file is left untouched.
    /// * **Corrupt / unreadable existing file** — worthless; the incoming
    ///   valid snapshot replaces it via the atomic temp→rename path.
    /// * **No existing file** — normal first-write path; atomic temp→rename.
    ///
    /// The actual publish path serialises to a sibling `.tmp` file, fsyncs,
    /// then renames into place.  No `.tmp` file is left behind on any branch.
    ///
    /// See docs/agent/world/ecs-runtime.md — Restore snapshots.
    pub fn write(&self, snapshot: &Snapshot) -> Result<(), Error> {
        std::fs::create_dir_all(&self.dir)?;
        let final_path = self.snapshot_path(snapshot.tick);

        // Non-destructive guard: inspect any existing file BEFORE touching disk.
        if final_path.exists() {
            match self.load_snapshot(snapshot.tick) {
                Ok(existing) if existing.validate() => {
                    if existing.digest == snapshot.digest {
                        // Identical valid snapshot on disk — idempotent no-op.
                        return Ok(());
                    } else {
                        // Same tick, different digest from a VALID existing snapshot.
                        // A deterministic snapshot store must never yield two
                        // distinct valid snapshots for the same tick — this signals
                        // a bug or inconsistency in the caller.
                        return Err(Error::World(format!(
                            "snapshot at tick {} already exists with digest {} but \
                             write was called with digest {}; refusing to clobber",
                            snapshot.tick, existing.digest.0, snapshot.digest.0
                        )));
                    }
                }
                // Corrupt or unreadable — the existing file is worthless; fall
                // through and replace it with the valid incoming snapshot.
                _ => {}
            }
        }

        // Atomic publish: write to .tmp, fsync, rename into place.
        let tmp_path = self.dir.join(format!("{}.snapshot.json.tmp", snapshot.tick));
        let result = (|| -> Result<(), Error> {
            let mut file = std::fs::File::create(&tmp_path)?;
            let json = serde_json::to_vec(snapshot)?;
            file.write_all(&json)?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &final_path)?;
            Ok(())
        })();
        if result.is_err() {
            // Best-effort cleanup: leave no stray .tmp file on any error branch.
            let _ = std::fs::remove_file(&tmp_path);
        }
        result
    }

    /// Return the max-tick valid snapshot at or before `target`.
    ///
    /// Enumerates all `<N>.snapshot.json` files in `dir` with tick ≤ `target`,
    /// sorts them descending by tick, and returns the FIRST one that passes
    /// `Snapshot::validate()` (digest recompute). Files that fail to parse or
    /// whose digest does not match are skipped without deletion.
    ///
    /// Returns `Ok(None)` when no valid snapshot exists at or before `target`;
    /// the caller should fall back to genesis + full log replay (Inv 9).
    ///
    /// See docs/agent/world/ecs-runtime.md — Restore snapshots (VC-2.3).
    pub fn nearest_at_or_before(&self, target: Tick) -> Result<Option<Snapshot>, Error> {
        let mut candidates = self.list_ticks_at_or_before(target)?;
        // Descend from highest tick so the most-recent valid snapshot wins.
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        for tick in candidates {
            match self.load_snapshot(tick) {
                Ok(snap) if snap.validate() => return Ok(Some(snap)),
                // IO/parse errors and digest mismatches are both treated as
                // corruption — skip silently, never delete, never panic.
                _ => continue,
            }
        }
        Ok(None)
    }

    /// Prune snapshots using the keep-K retention policy.
    ///
    /// Delegates to `retention::reclaim` which calls `evictable_snapshots`
    /// (pure policy) and then removes each evictable file. `keep == 0` means
    /// unbounded — no snapshot is removed. `live_tail` is the current live
    /// frontier of the session (the highest recorded tick); snapshots beyond
    /// it are always protected.
    ///
    /// Log-segment cold-tiering (floor computation + segment moving) is wired
    /// in the CR-segment task; this method covers snapshot file retention only.
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / snapshot retention.
    pub fn prune(&self, keep: u32, live_tail: Tick) -> Result<Vec<Tick>, Error> {
        crate::agent::world::retention::reclaim(self, keep, live_tail)
    }

    /// List every tick for which a snapshot file exists in `dir`.
    ///
    /// Used by the retention policy (`retention::reclaim`) to build the full
    /// candidate set before computing the evictable subset. Returns an empty
    /// vec when the store directory does not yet exist.
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / reclaim shell.
    pub fn list_ticks(&self) -> Result<Vec<Tick>, Error> {
        self.list_ticks_at_or_before(Tick::MAX)
    }

    /// Delete the snapshot file for `tick` from the store.
    ///
    /// This is the minimal guarded deletion seam used by the retention reclaim
    /// shell (`retention::reclaim`). It removes only the specific
    /// `<tick>.snapshot.json` file; no other file in the store is touched.
    ///
    /// Callers must only delete ticks returned by `retention::evictable_snapshots`
    /// — those snapshots are regenerable from an earlier snapshot/genesis plus
    /// the event-log tail (Inv 9/11), so deletion loses no unique data.
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / reclaim shell.
    pub fn remove(&self, tick: Tick) -> Result<(), Error> {
        let path = self.snapshot_path(tick);
        std::fs::remove_file(&path)?;
        Ok(())
    }

    // --- private helpers ---

    fn snapshot_path(&self, tick: Tick) -> PathBuf {
        self.dir.join(format!("{tick}.snapshot.json"))
    }

    /// Return every tick for which a valid-looking snapshot file exists in
    /// `dir` with tick ≤ `target`.  Malformed filenames are silently skipped.
    fn list_ticks_at_or_before(&self, target: Tick) -> Result<Vec<Tick>, Error> {
        match std::fs::read_dir(&self.dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Error::Io(e)),
            Ok(entries) => {
                let mut ticks = Vec::new();
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name();
                    if let Some(tick) = parse_snapshot_filename(&name.to_string_lossy())
                        && tick <= target
                    {
                        ticks.push(tick);
                    }
                }
                Ok(ticks)
            }
        }
    }

    fn load_snapshot(&self, tick: Tick) -> Result<Snapshot, Error> {
        let path = self.snapshot_path(tick);
        let bytes = std::fs::read(&path)?;
        let snap: Snapshot = serde_json::from_slice(&bytes)?;
        Ok(snap)
    }
}

/// Parse `"<tick>.snapshot.json"` → `Tick`, returning `None` for any other
/// filename shape so unrelated directory entries are silently ignored.
///
/// The tick integer is checked for canonical form: after parsing, it is
/// re-formatted and compared byte-by-byte against the original filename.
/// Non-canonical aliases like `"01.snapshot.json"` (which would parse to
/// tick 1 but round-trip to `"1.snapshot.json"`) are rejected here so that
/// `list_ticks` never emits duplicate ticks for the same canonical file.
fn parse_snapshot_filename(name: &str) -> Option<Tick> {
    let tick_str = name.strip_suffix(".snapshot.json")?;
    let tick: Tick = tick_str.parse().ok()?;
    // Canonical round-trip guard: the re-formatted filename must match the
    // original byte-for-byte.  Rejects leading-zero aliases and any other
    // non-canonical representation that would alias an existing canonical file.
    if format!("{tick}.snapshot.json") != name {
        return None;
    }
    Some(tick)
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
    use crate::agent::world::inputs::{Fingerprint, LogicalInput, Origin};
    use crate::agent::world::replay::fold_log as replay_fold_log;
    use crate::agent::world::world::{
        Activity, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources,
    };

    const SEED: u64 = 42;

    fn offline_model() -> ModelConfig {
        ModelConfig {
            model: "claude-snapshot-unit".into(),
            max_tokens: 512,
            effort: Effort::Low,
        }
    }

    /// Minimal genesis matching the canonical test genesis in replay.rs.
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

    /// `capture` produces a Snapshot whose `tick` matches `World::clock` and
    /// whose `digest` is a non-empty hex string.
    #[test]
    fn capture_sets_tick_and_digest() {
        let genesis = make_genesis(SEED);
        let snap = capture(genesis.clone());
        assert_eq!(snap.tick, genesis.clock, "tick must equal World::clock");
        assert!(!snap.digest.0.is_empty(), "digest must be non-empty");
    }

    /// `validate` returns `true` for an honest snapshot.
    #[test]
    fn validate_accepts_honest_snapshot() {
        let snap = capture(make_genesis(SEED));
        assert!(snap.validate(), "validate must accept an honest snapshot");
    }

    /// `validate` returns `false` after the digest is tampered.
    #[test]
    fn validate_rejects_tampered_digest() {
        let mut snap = capture(make_genesis(SEED));
        snap.digest = Fingerprint("0000000000000000000000000000000000000000000000000000000000000000".into());
        assert!(!snap.validate(), "validate must reject a tampered digest");
    }

    /// A Snapshot serde-round-trips faithfully.
    #[test]
    fn snapshot_serde_round_trip() {
        let snap = capture(make_genesis(SEED));
        let json = serde_json::to_string(&snap).expect("serialize Snapshot");
        let restored: Snapshot = serde_json::from_str(&json).expect("deserialize Snapshot");
        assert_eq!(snap, restored, "Snapshot must round-trip through JSON");
        assert!(restored.validate(), "round-tripped Snapshot must still validate");
    }

    /// `snapshot_at(events, build_genesis, T).world` is byte-identical to
    /// `fold_log(genesis, events[..=T])` — the digest-equals-fold property
    /// (VC-2.1).
    #[test]
    fn snapshot_at_world_equals_fold_log() {
        let events = vec![
            session_started_event(),
            user_message_event(1, "hello"),
            user_message_event(2, "world"),
        ];

        let snap = snapshot_at(&events, make_genesis, 1)
            .expect("snapshot_at must succeed on a valid log");

        // Reproduce what fold_log(genesis, events[..=1]) gives.
        let genesis = make_genesis(SEED);
        let prefix: Vec<Event> = events.iter().filter(|e| e.at <= 1).cloned().collect();
        let expected_world = replay_fold_log(genesis, &prefix);

        assert_eq!(
            serde_json::to_vec(&snap.world).expect("serialize snap.world"),
            serde_json::to_vec(&expected_world).expect("serialize expected_world"),
            "snapshot_at world must be byte-identical to fold_log(genesis, events[..=T])"
        );
        assert_eq!(snap.world, expected_world, "worlds must be structurally equal");
    }

    /// `snapshot_at` trims events past `tick`: the world at tick 1 must differ
    /// from the world at tick 2 if the extra event mutates state.
    #[test]
    fn snapshot_at_trims_future_events() {
        let events = vec![
            session_started_event(),
            user_message_event(1, "hello"),
            user_message_event(2, "world"),
        ];

        let snap_t1 = snapshot_at(&events, make_genesis, 1).expect("snap at tick 1");
        let snap_t2 = snapshot_at(&events, make_genesis, 2).expect("snap at tick 2");

        assert_ne!(
            snap_t1.world, snap_t2.world,
            "worlds at different ticks must differ when an event mutates state"
        );
    }

    /// `snapshot_at` errors on a log with no `SessionStarted` header.
    #[test]
    fn snapshot_at_errors_without_session_started() {
        let events = vec![user_message_event(1, "no header")];
        let result = snapshot_at(&events, make_genesis, 1);
        assert!(
            result.is_err(),
            "snapshot_at must fail when the log has no SessionStarted"
        );
    }

    // -----------------------------------------------------------------------
    // parse_snapshot_filename canonical-form tests
    // -----------------------------------------------------------------------

    /// `parse_snapshot_filename` accepts well-formed canonical filenames and
    /// returns the correct tick value.
    #[test]
    fn parse_snapshot_filename_accepts_canonical_names() {
        assert_eq!(super::parse_snapshot_filename("0.snapshot.json"), Some(0));
        assert_eq!(super::parse_snapshot_filename("1.snapshot.json"), Some(1));
        assert_eq!(
            super::parse_snapshot_filename("18446744073709551615.snapshot.json"),
            Some(u64::MAX)
        );
    }

    /// `parse_snapshot_filename` rejects non-canonical aliases such as
    /// `"01.snapshot.json"` (leading zero) so that `list_ticks` cannot produce
    /// duplicate ticks aliasing the same canonical snapshot file.
    #[test]
    fn parse_snapshot_filename_rejects_leading_zero_aliases() {
        assert_eq!(
            super::parse_snapshot_filename("01.snapshot.json"),
            None,
            "leading-zero alias must be rejected to prevent duplicate ticks"
        );
        assert_eq!(
            super::parse_snapshot_filename("007.snapshot.json"),
            None,
            "leading-zero alias must be rejected"
        );
        assert_eq!(
            super::parse_snapshot_filename("00.snapshot.json"),
            None,
            "leading-zero alias for tick 0 must be rejected"
        );
    }

    /// `parse_snapshot_filename` returns `None` for unrelated filenames.
    #[test]
    fn parse_snapshot_filename_rejects_unrelated_names() {
        assert_eq!(super::parse_snapshot_filename("not-a-snapshot.json"), None);
        assert_eq!(super::parse_snapshot_filename("1.snapshot.json.tmp"), None);
        assert_eq!(super::parse_snapshot_filename("abc.snapshot.json"), None);
        assert_eq!(super::parse_snapshot_filename(""), None);
    }

    // -----------------------------------------------------------------------
    // SnapshotStore tests
    // -----------------------------------------------------------------------

    /// `SnapshotStore::write` followed by `nearest_at_or_before(tick)` returns
    /// the written snapshot faithfully (write + read round-trip).
    #[test]
    fn store_write_read_round_trip() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let snap = capture(make_genesis(SEED));

        store.write(&snap).expect("write snapshot");

        let loaded = store
            .nearest_at_or_before(snap.tick)
            .expect("nearest_at_or_before")
            .expect("must find the written snapshot");

        assert_eq!(snap, loaded, "loaded snapshot must equal the written one");
        assert!(loaded.validate(), "loaded snapshot must validate");
    }

    /// `nearest_at_or_before` returns the MAX-tick valid snapshot ≤ target when
    /// multiple snapshots are present at different ticks.
    #[test]
    fn store_nearest_at_or_before_selects_max_tick() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let events = vec![
            session_started_event(),
            user_message_event(1, "a"),
            user_message_event(2, "b"),
            user_message_event(3, "c"),
            user_message_event(4, "d"),
        ];

        // Write snapshots at ticks 1, 3, and 5 (tick 5 is past the query).
        let snap1 = snapshot_at(&events, make_genesis, 1).expect("snap at 1");
        let snap3 = snapshot_at(&events, make_genesis, 3).expect("snap at 3");
        store.write(&snap1).expect("write snap1");
        store.write(&snap3).expect("write snap3");

        // Querying at tick 4 must return the snapshot at tick 3 (highest ≤ 4).
        let result = store
            .nearest_at_or_before(4)
            .expect("nearest_at_or_before")
            .expect("must find a snapshot");
        assert_eq!(result.tick, 3, "must select tick 3 as the nearest ≤ 4");

        // Querying at tick 1 must return the snapshot at tick 1.
        let result = store
            .nearest_at_or_before(1)
            .expect("nearest_at_or_before")
            .expect("must find a snapshot");
        assert_eq!(result.tick, 1, "must select tick 1 as the nearest ≤ 1");

        // Querying at tick 0 (before all snapshots) must return None.
        let result = store
            .nearest_at_or_before(0)
            .expect("nearest_at_or_before");
        assert!(result.is_none(), "no snapshot exists at tick 0");
    }

    /// A byte-corrupted snapshot file is skipped in favour of the next-earlier
    /// valid snapshot.  When no earlier valid snapshot exists, `None` is
    /// returned (caller falls back to genesis).  No file is deleted (Inv 9).
    #[test]
    fn store_corrupt_snapshot_falls_back_to_earlier() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let snapshots_dir = dir.path().join("snapshots");
        let store = SnapshotStore::new(&snapshots_dir);
        let events = vec![
            session_started_event(),
            user_message_event(1, "a"),
            user_message_event(2, "b"),
        ];

        let snap1 = snapshot_at(&events, make_genesis, 1).expect("snap at 1");
        let snap2 = snapshot_at(&events, make_genesis, 2).expect("snap at 2");
        store.write(&snap1).expect("write snap1");
        store.write(&snap2).expect("write snap2");

        // Corrupt the tick-2 file: flip a byte so the digest no longer matches.
        let corrupt_path = snapshots_dir.join("2.snapshot.json");
        let mut bytes = std::fs::read(&corrupt_path).expect("read snap2 file");
        // Flip one byte in the middle of the file (guaranteed to be within the
        // world payload, past the JSON opening brace).
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&corrupt_path, &bytes).expect("write corrupted bytes");

        // Querying ≤ 2 must skip the corrupted tick-2 file and return tick-1.
        let result = store
            .nearest_at_or_before(2)
            .expect("nearest_at_or_before with a corrupt file");
        let snap = result.expect("must fall back to the valid tick-1 snapshot");
        assert_eq!(snap.tick, 1, "must fall back to the earlier valid snapshot");

        // The corrupt file must NOT have been deleted (log is source of truth).
        assert!(
            corrupt_path.exists(),
            "corrupt snapshot file must not be deleted by the loader"
        );

        // When ALL snapshots are corrupt, None is returned (fall back to genesis).
        let corrupt_path1 = snapshots_dir.join("1.snapshot.json");
        let mut bytes1 = std::fs::read(&corrupt_path1).expect("read snap1 file");
        let mid1 = bytes1.len() / 2;
        bytes1[mid1] ^= 0xFF;
        std::fs::write(&corrupt_path1, &bytes1).expect("write corrupted bytes for snap1");

        let result = store
            .nearest_at_or_before(2)
            .expect("nearest_at_or_before when all corrupt");
        assert!(
            result.is_none(),
            "must return None when no valid snapshot exists (caller falls back to genesis)"
        );
    }

    /// After a successful `write`, no `.tmp` file remains in the snapshots dir.
    #[test]
    fn store_no_partial_file_after_write() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let snapshots_dir = dir.path().join("snapshots");
        let store = SnapshotStore::new(&snapshots_dir);
        let snap = capture(make_genesis(SEED));

        store.write(&snap).expect("write snapshot");

        // Scan for any leftover .tmp file — there must be none.
        let tmp_files: Vec<_> = std::fs::read_dir(&snapshots_dir)
            .expect("read snapshots dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".snapshot.json.tmp")
            })
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp file must remain after a successful write: {tmp_files:?}"
        );

        // The real snapshot file must exist.
        let final_path = snapshots_dir.join(format!("{}.snapshot.json", snap.tick));
        assert!(final_path.exists(), "the snapshot file must exist after write");
    }

    /// A same-tick re-write of IDENTICAL content is an idempotent no-op: the
    /// second `write` returns `Ok(())` and exactly one snapshot file remains.
    #[test]
    fn store_write_idempotent_for_identical_content() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let snapshots_dir = dir.path().join("snapshots");
        let store = SnapshotStore::new(&snapshots_dir);
        let snap = capture(make_genesis(SEED));

        store.write(&snap).expect("first write must succeed");
        store
            .write(&snap)
            .expect("second write of identical content must be an idempotent no-op");

        // Exactly one snapshot file must exist (no .tmp, no duplicate).
        let snapshot_files: Vec<_> = std::fs::read_dir(&snapshots_dir)
            .expect("read snapshots dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".snapshot.json"))
            .collect();
        assert_eq!(
            snapshot_files.len(),
            1,
            "exactly one snapshot file must exist after idempotent writes"
        );
    }

    /// A same-tick write of DIFFERENT valid content errors rather than
    /// clobbering: the original snapshot is preserved, and no .tmp is left.
    #[test]
    fn store_write_errors_on_different_content_same_tick() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let snapshots_dir = dir.path().join("snapshots");
        let store = SnapshotStore::new(&snapshots_dir);

        // snap_a: genesis with SEED — tick 0.
        let snap_a = capture(make_genesis(SEED));

        // snap_b: genesis with a different seed — different world, different
        // digest, but the SAME tick (both genesis worlds start at clock 0).
        let snap_b = capture(make_genesis(SEED + 1));
        assert_eq!(snap_a.tick, snap_b.tick, "both genesis snapshots must share tick 0");
        assert_ne!(snap_a.digest, snap_b.digest, "different seeds must yield different digests");

        store.write(&snap_a).expect("first write must succeed");

        // Writing a different valid snapshot at the same tick must error.
        let result = store.write(&snap_b);
        assert!(
            result.is_err(),
            "write with different valid content at the same tick must return Err; got: {result:?}"
        );

        // The original snapshot must be intact — not clobbered.
        let loaded = store
            .nearest_at_or_before(snap_a.tick)
            .expect("nearest_at_or_before")
            .expect("original snapshot must still exist");
        assert_eq!(
            loaded.digest, snap_a.digest,
            "original snapshot digest must be unchanged after a refused overwrite"
        );

        // No .tmp file must remain.
        let tmp_files: Vec<_> = std::fs::read_dir(&snapshots_dir)
            .expect("read snapshots dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".snapshot.json.tmp"))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp file must remain after a refused overwrite: {tmp_files:?}"
        );
    }
}
