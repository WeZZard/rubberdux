// The segmented log + cold-tiering seam is consumed by the prune CLI
// (CR-cli-prune) and the dead-branch reclaimer (CR-reclaim); the binary target
// has no caller yet, so silence dead-code lints until that wiring lands.
#![allow(dead_code)]

//! Segmented event log with cold-storage tiering.
//!
//! [`SegmentedEventLog`] is an [`EventLog`] whose stratum-1 stream is split into
//! tick-contiguous **segments** described by a `manifest.json`. It seals the
//! open segment and rolls to a fresh one at snapshot boundaries
//! ([`SegmentedEventLog::seal_and_roll`]); once a sealed segment is fully covered
//! by a retained snapshot it is **moved** (never deleted) to a `cold/` directory
//! ([`SegmentedEventLog::tier_to_cold`]) so the log stays bounded on the hot path
//! yet remains the single source of truth (Inv 9).
//!
//! The module mirrors the pure-policy / thin-IO-shell split used across the
//! runtime (cf. `retention.rs`): [`cold_storable`] is a PURE function over the
//! manifest plus a protection `floor` (the `first_tick` of the oldest retained
//! snapshot) — it selects only SEALED segments fully covered below the floor —
//! and the tiering methods are the thin shell that performs the move.
//!
//! A legacy single `world-events.jsonl` with no `manifest.json` (an M1 session,
//! or a fresh segmented log before its first roll) loads as ONE implicit Hot
//! segment, so M1 logs load unchanged. The log is append-only: there is no
//! rewrite or truncate path, and tiering moves bytes without altering them, so
//! [`SegmentedEventLog::load`] returns the full ordered log byte-identically
//! before and after tiering.
//!
//! See docs/agent/world/ecs-runtime.md — PRUNE / segmented log and cold-storage
//! tiering (Inv 9).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::world::event_log::{append_jsonl, load_jsonl, EventLog};
use crate::agent::world::inputs::Event;
use crate::agent::world::lifecycle::LifecycleEvent;
use crate::agent::world::world::Tick;
use crate::error::Error;

/// The canonical name of a session's first (and, until the first roll, only)
/// segment file — the same file an M1 single-file log writes, so a fresh
/// segmented log is byte-compatible with M1 until it seals its first segment.
const PRIMARY_SEGMENT: &str = "world-events";

/// The sibling file holding the stratum-2 operational stream. It is NOT
/// segmented: stratum 2 is read only by the resume path and never cold-tiered,
/// so it keeps the same `<primary>.lifecycle.jsonl` sibling an M1 session used.
const LIFECYCLE_FILE: &str = "world-events.lifecycle.jsonl";

/// The subdirectory holding cold-tiered segments. Tiering MOVES a fully-covered
/// sealed segment here; the segment is never deleted (Inv 9).
const COLD_DIR: &str = "cold";

/// The manifest file recording the segment layout.
const MANIFEST_FILE: &str = "manifest.json";

// ---------------------------------------------------------------------------
// Manifest — the segment layout
// ---------------------------------------------------------------------------

/// A segment's identifier, which is also the stem of its on-disk file
/// (`<id>.jsonl` in the hot dir, `cold/<id>.jsonl` once tiered). The first
/// segment carries the M1-compatible id `world-events`; segments opened by a
/// roll carry `seg-<first_tick>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentId(pub String);

/// Which storage tier a segment lives in. `Hot` segments sit in the session
/// directory; `Cold` segments have been moved to the `cold/` subdirectory. A
/// segment is NEVER deleted — only moved between tiers (Inv 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Hot,
    Cold,
}

/// One segment's manifest entry: its id, the inclusive tick range it covers,
/// and its tier. `first_tick`/`last_tick` are recomputed from the segment's
/// actual events when it is sealed, so a sealed segment's `last_tick` is the
/// exact upper bound [`cold_storable`] compares against the floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub id: SegmentId,
    pub first_tick: Tick,
    pub last_tick: Tick,
    pub tier: Tier,
}

/// The segment layout backing a [`SegmentedEventLog`]. Segments are stored in
/// creation order, so the LAST entry is always the open (active) segment that
/// appends land in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub segments: Vec<SegmentEntry>,
}

// ---------------------------------------------------------------------------
// Pure policy — cold_storable
// ---------------------------------------------------------------------------

/// Select the SEALED segments that are fully covered below the protection
/// `floor` and may therefore be cold-tiered.
///
/// A segment is eligible iff it is (1) not the open/active segment — only sealed
/// segments are immutable enough to move; (2) still `Hot` — already-cold
/// segments are skipped (idempotence); and (3) fully covered, i.e.
/// `last_tick < floor`. `floor` is the `first_tick` of the OLDEST retained
/// snapshot: any segment ending strictly before it is regenerable from that
/// snapshot plus the hot tail, so moving it loses nothing (Inv 9).
///
/// A `floor` of `0` protects every segment (no `last_tick` is `< 0`), matching
/// the "no retained snapshot ⇒ tier nothing" case.
///
/// This is a PURE function: it reads the manifest and floor only, performs no
/// I/O, and is unit-testable in isolation. The move itself is the thin shell
/// [`SegmentedEventLog::tier_to_cold`].
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / cold-storage tiering.
pub fn cold_storable(manifest: &Manifest, floor: Tick) -> Vec<SegmentId> {
    // The open segment is the last entry; it is mutable (appends land in it) and
    // must never be tiered.
    let active_id = manifest.segments.last().map(|s| &s.id);
    manifest
        .segments
        .iter()
        .filter(|s| Some(&s.id) != active_id)
        .filter(|s| s.tier == Tier::Hot)
        .filter(|s| s.last_tick < floor)
        .map(|s| s.id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// SegmentedEventLog
// ---------------------------------------------------------------------------

/// An append-only [`EventLog`] whose stratum-1 stream is split into tick-ordered
/// segments, with fully-covered sealed segments cold-tiered to a `cold/` dir.
///
/// `load` concatenates every segment's events in `first_tick` order
/// (cold-then-hot, since cold segments are the oldest), reproducing exactly what
/// a single-file log would return — byte-identically before and after tiering.
/// Stratum-2 lifecycle records stay in a single sibling file, unsegmented.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / segmented log.
pub struct SegmentedEventLog {
    root: PathBuf,
    manifest: Manifest,
}

impl SegmentedEventLog {
    /// Open the segmented log rooted at `root`.
    ///
    /// If a `manifest.json` is present it is loaded as-is. Otherwise the single
    /// `world-events.jsonl` (whether it exists yet or not) is adopted as ONE
    /// implicit Hot segment — covering both a legacy M1 session and a fresh
    /// segmented log before its first roll. Opening is side-effect-free: no
    /// `manifest.json` is written for a manifest-less log, so an M1 session
    /// loads unchanged.
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / segmented log.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        let manifest_path = root.join(MANIFEST_FILE);
        let manifest = if manifest_path.exists() {
            let bytes = std::fs::read(&manifest_path)?;
            serde_json::from_slice(&bytes)?
        } else {
            // No manifest: the single primary file is the sole Hot segment. Its
            // coverage is read from the file so a subsequent seal is accurate;
            // an absent/empty file yields an empty [0, 0] segment.
            let path = root.join(format!("{PRIMARY_SEGMENT}.jsonl"));
            let (first_tick, last_tick) = segment_bounds(&path)?.unwrap_or((0, 0));
            Manifest {
                segments: vec![SegmentEntry {
                    id: SegmentId(PRIMARY_SEGMENT.into()),
                    first_tick,
                    last_tick,
                    tier: Tier::Hot,
                }],
            }
        };
        Ok(Self { root, manifest })
    }

    /// The current segment layout — the seam the prune CLI / reclaimer inspect.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Seal the open segment at a snapshot boundary and roll to a fresh one.
    ///
    /// The open segment's `first_tick`/`last_tick` are recomputed from its actual
    /// events and frozen, then a new empty Hot segment `seg-<at_tick>` is opened
    /// as the append target. The manifest is persisted atomically. This is the
    /// only place a segment boundary is created; it never rewrites segment data.
    ///
    /// A no-op when the open segment is empty (nothing to seal) or when `at_tick`
    /// is not strictly past the open segment's start (no forward boundary to
    /// draw), so repeated calls without intervening appends do not proliferate
    /// empty segments.
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / segmented log (seal-and-roll).
    pub fn seal_and_roll(&mut self, at_tick: Tick) -> Result<(), Error> {
        let active_idx = self
            .manifest
            .segments
            .len()
            .checked_sub(1)
            .ok_or_else(|| Error::World("segmented log has no segment to seal".into()))?;

        let active = &self.manifest.segments[active_idx];
        let path = self.segment_path(active);
        let Some((first_tick, last_tick)) = segment_bounds(&path)? else {
            // The open segment holds no events — nothing to seal.
            return Ok(());
        };
        if at_tick <= first_tick {
            // No forward boundary to draw past the segment's own start.
            return Ok(());
        }

        // Freeze the sealed segment's real coverage, then open the next segment.
        self.manifest.segments[active_idx].first_tick = first_tick;
        self.manifest.segments[active_idx].last_tick = last_tick;
        self.manifest.segments.push(SegmentEntry {
            id: SegmentId(format!("seg-{at_tick}")),
            first_tick: at_tick,
            last_tick: at_tick,
            tier: Tier::Hot,
        });
        self.persist_manifest()
    }

    /// MOVE a fully-covered sealed segment from the hot directory to `cold/` and
    /// mark it `Cold` in the manifest. The segment is NEVER deleted (Inv 9).
    ///
    /// The move is a same-filesystem `rename` when possible, falling back to
    /// copy + fsync + rename-into-place + remove-source across a tier boundary.
    /// Refuses to tier the open (active) segment, and is idempotent on a segment
    /// already `Cold`. The manifest is persisted only after the bytes have
    /// landed in `cold/`, so a crash cannot leave the manifest pointing at an
    /// absent hot file (and `load` resolves either tier defensively regardless).
    ///
    /// Callers should only pass ids returned by [`cold_storable`].
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / cold-storage tiering.
    pub fn tier_to_cold(&mut self, id: &SegmentId) -> Result<(), Error> {
        let idx = self
            .manifest
            .segments
            .iter()
            .position(|s| &s.id == id)
            .ok_or_else(|| Error::World(format!("no segment with id {}", id.0)))?;

        if idx + 1 == self.manifest.segments.len() {
            return Err(Error::World(format!(
                "refusing to tier the open (active) segment {}",
                id.0
            )));
        }
        if self.manifest.segments[idx].tier == Tier::Cold {
            // Already cold — idempotent.
            return Ok(());
        }

        let src = self.hot_path(id);
        let dst = self.cold_path(id);
        move_file(&src, &dst)?;

        // Flip the tier only after the bytes are safely in cold/.
        self.manifest.segments[idx].tier = Tier::Cold;
        self.persist_manifest()
    }

    /// Thin IO shell over [`cold_storable`]: move every sealed segment fully
    /// covered below `floor` to `cold/`, returning the ids moved.
    ///
    /// This is the composition the prune CLI / reclaimer drive — pure plan
    /// ([`cold_storable`]) then the per-segment move ([`tier_to_cold`]).
    ///
    /// See docs/agent/world/ecs-runtime.md — PRUNE / cold-storage tiering.
    pub fn tier_cold_storable(&mut self, floor: Tick) -> Result<Vec<SegmentId>, Error> {
        let movable = cold_storable(&self.manifest, floor);
        for id in &movable {
            self.tier_to_cold(id)?;
        }
        Ok(movable)
    }

    // --- private helpers ---

    /// The hot-tier path for a segment id: `<root>/<id>.jsonl`.
    fn hot_path(&self, id: &SegmentId) -> PathBuf {
        self.root.join(format!("{}.jsonl", id.0))
    }

    /// The cold-tier path for a segment id: `<root>/cold/<id>.jsonl`.
    fn cold_path(&self, id: &SegmentId) -> PathBuf {
        self.root.join(COLD_DIR).join(format!("{}.jsonl", id.0))
    }

    /// The path a segment currently lives at, per its recorded tier.
    fn segment_path(&self, seg: &SegmentEntry) -> PathBuf {
        match seg.tier {
            Tier::Hot => self.hot_path(&seg.id),
            Tier::Cold => self.cold_path(&seg.id),
        }
    }

    /// The path to read a segment from, tolerating a crash mid-tiering: a
    /// segment is never deleted, only moved, so if it is absent at its recorded
    /// tier it must be at the other one.
    fn resolve_segment_path(&self, seg: &SegmentEntry) -> PathBuf {
        let primary = self.segment_path(seg);
        if primary.exists() {
            return primary;
        }
        let alt = match seg.tier {
            Tier::Hot => self.cold_path(&seg.id),
            Tier::Cold => self.hot_path(&seg.id),
        };
        if alt.exists() { alt } else { primary }
    }

    fn lifecycle_path(&self) -> PathBuf {
        self.root.join(LIFECYCLE_FILE)
    }

    /// Persist the manifest atomically (temp + fsync + rename) so a partial
    /// write never corrupts the layout the log is reconstructed from.
    fn persist_manifest(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.root)?;
        let final_path = self.root.join(MANIFEST_FILE);
        let tmp_path = self.root.join(format!("{MANIFEST_FILE}.tmp"));
        let result = (|| -> Result<(), Error> {
            let json = serde_json::to_vec_pretty(&self.manifest)?;
            let mut file = File::create(&tmp_path)?;
            file.write_all(&json)?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &final_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        result
    }
}

impl EventLog for SegmentedEventLog {
    fn append(&mut self, event: &Event) -> Result<(), Error> {
        let path = {
            let active = self
                .manifest
                .segments
                .last()
                .ok_or_else(|| Error::World("segmented log has no active segment".into()))?;
            self.segment_path(active)
        };
        append_jsonl(&path, event)
    }

    fn load(&self) -> Result<Vec<Event>, Error> {
        // Order by first_tick so cold (oldest) segments precede hot ones and the
        // open segment (highest first_tick) comes last — the full log in tick
        // order, identical to a single-file log.
        let mut segments: Vec<&SegmentEntry> = self.manifest.segments.iter().collect();
        segments.sort_by_key(|s| s.first_tick);

        let mut events = Vec::new();
        for seg in segments {
            let path = self.resolve_segment_path(seg);
            let mut chunk = load_jsonl::<Event>(&path, "event log")?;
            events.append(&mut chunk);
        }
        Ok(events)
    }

    fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error> {
        append_jsonl(&self.lifecycle_path(), event)
    }

    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
        load_jsonl(&self.lifecycle_path(), "lifecycle log")
    }
}

// ---------------------------------------------------------------------------
// Free helpers (pure / thin IO)
// ---------------------------------------------------------------------------

/// The inclusive `(first_tick, last_tick)` a segment file covers, or `None` when
/// it holds no events (missing/empty file). Used at seal time to freeze a
/// sealed segment's exact coverage.
fn segment_bounds(path: &Path) -> Result<Option<(Tick, Tick)>, Error> {
    let events = load_jsonl::<Event>(path, "event log")?;
    let mut iter = events.iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    let mut min = first.at;
    let mut max = first.at;
    for ev in iter {
        if ev.at < min {
            min = ev.at;
        }
        if ev.at > max {
            max = ev.at;
        }
    }
    Ok(Some((min, max)))
}

/// Move `src` to `dst`, creating `dst`'s parent. A same-filesystem `rename` is
/// the fast, atomic path; across a tier boundary it falls back to
/// copy + fsync + rename-into-place + remove-source. Either way the bytes are
/// preserved and the source is gone only AFTER the destination is durable —
/// the segment is moved, never lost.
fn move_file(src: &Path, dst: &Path) -> Result<(), Error> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Cross-device fallback: copy to a temp beside the destination, fsync it,
    // rename it into place, then remove the source.
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".cold-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::copy(src, &tmp)?;
    {
        let file = File::open(&tmp)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, dst)?;
    std::fs::remove_file(src)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::world::inputs::{LogicalInput, Origin};

    fn session_started() -> Event {
        Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input: LogicalInput::SessionStarted {
                seed: 0xCAFE_F00D,
                surface_tools: Vec::new(),
            },
        }
    }

    fn user_message(at: Tick, text: &str) -> Event {
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

    /// A short canonical log: a `SessionStarted` header plus user messages.
    fn sample_events() -> Vec<Event> {
        vec![
            session_started(),
            user_message(1, "alpha"),
            user_message(2, "beta"),
            user_message(3, "gamma"),
        ]
    }

    // -----------------------------------------------------------------------
    // append + load round-trip (+ durability across reopen)
    // -----------------------------------------------------------------------

    #[test]
    fn append_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("world");
        let events = sample_events();

        let mut log = SegmentedEventLog::open(&root).expect("open fresh log");
        for e in &events {
            log.append(e).expect("append event");
        }
        assert_eq!(
            log.load().expect("load"),
            events,
            "load must return the appended sequence in order"
        );

        // Durable: a fresh handle over the same root reloads identically. No
        // roll happened, so this exercises the manifest-less (single Hot
        // segment) open path too.
        let reopened = SegmentedEventLog::open(&root).expect("reopen log");
        assert_eq!(
            reopened.load().expect("reload"),
            events,
            "a reopened log must reload the same sequence"
        );
        assert!(
            !root.join(MANIFEST_FILE).exists(),
            "an un-rolled log must not have written a manifest.json"
        );
    }

    // -----------------------------------------------------------------------
    // seal_and_roll produces a new segment
    // -----------------------------------------------------------------------

    #[test]
    fn seal_and_roll_produces_a_new_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("world");

        let mut log = SegmentedEventLog::open(&root).expect("open");
        for e in &sample_events() {
            log.append(e).expect("append pre-roll");
        }
        assert_eq!(log.manifest().segments.len(), 1, "one open segment so far");

        // Seal the open segment at the snapshot boundary and append past it.
        log.seal_and_roll(4).expect("seal and roll");
        log.append(&user_message(4, "delta")).expect("append post-roll");
        log.append(&user_message(5, "epsilon")).expect("append post-roll");

        // A new, distinct segment now exists alongside the sealed one.
        assert_eq!(
            log.manifest().segments.len(),
            2,
            "seal_and_roll must add exactly one new segment"
        );
        assert_eq!(log.manifest().segments[0].id.0, PRIMARY_SEGMENT);
        assert_eq!(log.manifest().segments[0].last_tick, 3, "sealed coverage frozen");
        assert_eq!(log.manifest().segments[1].id.0, "seg-4");

        // Both segment files and the manifest exist on disk.
        assert!(root.join("world-events.jsonl").exists());
        assert!(root.join("seg-4.jsonl").exists());
        assert!(root.join(MANIFEST_FILE).exists());

        // The full log still loads in order across the two segments.
        let mut expected = sample_events();
        expected.push(user_message(4, "delta"));
        expected.push(user_message(5, "epsilon"));
        assert_eq!(
            log.load().expect("load across segments"),
            expected,
            "load must concatenate segments in tick order"
        );
    }

    /// Re-sealing an empty open segment is a no-op (no empty-segment churn).
    #[test]
    fn seal_and_roll_on_empty_segment_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("world");
        let mut log = SegmentedEventLog::open(&root).expect("open");

        // Nothing appended yet — the open segment is empty.
        log.seal_and_roll(4).expect("roll on empty");
        assert_eq!(
            log.manifest().segments.len(),
            1,
            "rolling an empty segment must not add a segment"
        );
    }

    // -----------------------------------------------------------------------
    // cold_storable is a pure policy over the manifest + floor
    // -----------------------------------------------------------------------

    #[test]
    fn cold_storable_picks_only_sealed_segments_below_floor() {
        let manifest = Manifest {
            segments: vec![
                SegmentEntry {
                    id: SegmentId("world-events".into()),
                    first_tick: 0,
                    last_tick: 3,
                    tier: Tier::Hot,
                },
                SegmentEntry {
                    id: SegmentId("seg-4".into()),
                    first_tick: 4,
                    last_tick: 7,
                    tier: Tier::Hot,
                },
                SegmentEntry {
                    id: SegmentId("seg-8".into()),
                    first_tick: 8,
                    last_tick: 11,
                    tier: Tier::Cold, // already tiered — must be skipped
                },
                SegmentEntry {
                    id: SegmentId("seg-12".into()),
                    first_tick: 12,
                    last_tick: 12,
                    tier: Tier::Hot, // the OPEN segment — never tiered
                },
            ],
        };

        // floor 8: both sealed hot segments end below it.
        assert_eq!(
            cold_storable(&manifest, 8),
            vec![SegmentId("world-events".into()), SegmentId("seg-4".into())],
            "floor 8 selects the two fully-covered sealed hot segments"
        );

        // floor 7 (== seg-4.last_tick): strict `<` excludes seg-4.
        assert_eq!(
            cold_storable(&manifest, 7),
            vec![SegmentId("world-events".into())],
            "last_tick == floor must NOT be selected (strict `last_tick < floor`)"
        );

        // floor 4: only world-events ends below 4.
        assert_eq!(
            cold_storable(&manifest, 4),
            vec![SegmentId("world-events".into())],
        );

        // floor 0 protects everything.
        assert!(
            cold_storable(&manifest, 0).is_empty(),
            "a zero floor tiers nothing"
        );
    }

    // -----------------------------------------------------------------------
    // tiering MOVES (never deletes) and load() stays byte-identical
    // -----------------------------------------------------------------------

    #[test]
    fn tiering_moves_segment_and_keeps_load_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("world");

        // Build three segments: world-events [0..3], seg-4 [4..5], seg-6 [6..].
        let mut log = SegmentedEventLog::open(&root).expect("open");
        for e in &sample_events() {
            log.append(e).expect("append");
        }
        log.seal_and_roll(4).expect("roll at 4");
        log.append(&user_message(4, "delta")).expect("append");
        log.append(&user_message(5, "epsilon")).expect("append");
        log.seal_and_roll(6).expect("roll at 6");
        log.append(&user_message(6, "zeta")).expect("append");

        let before = log.load().expect("load before tiering");
        let before_bytes = serde_json::to_vec(&before).expect("serialize before");

        // floor 6: world-events (last 3) and seg-4 (last 5) are fully covered;
        // seg-6 is the open segment.
        let movable = cold_storable(log.manifest(), 6);
        assert_eq!(
            movable,
            vec![SegmentId("world-events".into()), SegmentId("seg-4".into())],
            "both sealed segments below floor 6 are cold-storable"
        );

        let moved = log.tier_cold_storable(6).expect("tier cold-storable");
        assert_eq!(moved, movable, "tier shell moves exactly the cold-storable set");

        for id in &moved {
            let hot = root.join(format!("{}.jsonl", id.0));
            let cold = root.join(COLD_DIR).join(format!("{}.jsonl", id.0));
            // MOVED, not deleted: gone from hot, present in cold.
            assert!(!hot.exists(), "segment {} must leave the hot tier", id.0);
            assert!(cold.exists(), "segment {} must exist in the cold tier", id.0);
            // The manifest marks it Cold.
            let entry = log
                .manifest()
                .segments
                .iter()
                .find(|s| &s.id == id)
                .expect("moved segment still in manifest");
            assert_eq!(entry.tier, Tier::Cold, "moved segment marked Cold");
        }

        // The open segment stayed hot.
        assert!(root.join("seg-6.jsonl").exists(), "the open segment stays hot");

        // load() is byte-identical before and after tiering (the critical invariant).
        let after = log.load().expect("load after tiering");
        assert_eq!(after, before, "load must be unchanged after tiering");
        assert_eq!(
            serde_json::to_vec(&after).expect("serialize after"),
            before_bytes,
            "load must be BYTE-identical before and after tiering"
        );
    }

    // -----------------------------------------------------------------------
    // legacy single-file adapter loads as ONE Hot segment
    // -----------------------------------------------------------------------

    #[test]
    fn legacy_single_file_loads_as_one_hot_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("world");
        std::fs::create_dir_all(&root).expect("create root");

        // Simulate an M1 session: a bare world-events.jsonl, no manifest.json.
        let events = sample_events();
        let mut blob = String::new();
        for e in &events {
            blob.push_str(&serde_json::to_string(e).expect("encode event"));
            blob.push('\n');
        }
        std::fs::write(root.join("world-events.jsonl"), blob).expect("write legacy log");

        let log = SegmentedEventLog::open(&root).expect("open legacy log");

        // Exactly one implicit Hot segment, named after the legacy file.
        assert_eq!(log.manifest().segments.len(), 1, "one implicit segment");
        assert_eq!(log.manifest().segments[0].tier, Tier::Hot, "it is Hot");
        assert_eq!(log.manifest().segments[0].id.0, PRIMARY_SEGMENT);

        // It loads byte-identically to the recorded events.
        assert_eq!(log.load().expect("load legacy"), events);

        // Opening a legacy log must NOT create a manifest.json (M1 loads unchanged).
        assert!(
            !root.join(MANIFEST_FILE).exists(),
            "opening a legacy log must not write a manifest.json"
        );
    }
}
