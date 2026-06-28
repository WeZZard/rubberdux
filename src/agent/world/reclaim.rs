// The reachability GC seam is consumed by the prune CLI (CR-cli-prune); the
// binary target has no caller yet, so silence dead-code lints until that wiring
// lands. The pure policy (`reachable`/`reclaimable`) and the `pin`/`drop`
// mutators are exercised by the unit tests below in the meantime.
#![allow(dead_code)]

//! Dead-branch reachability GC: reclaim counterfactual branches that are
//! unreachable (dropped or aged out, with no live handle and no retained
//! descendant) while NEVER touching a reachable branch or the shared parent
//! prefix.
//!
//! The module mirrors the pure-policy / thin-IO-shell split used across the
//! runtime (cf. `retention.rs`, `segment.rs`):
//!
//! * [`reachable`] is a PURE function over the branch descriptor list plus a
//!   [`Roots`] seed set. It marks every branch that is structurally reachable
//!   from a root — the live session, a pinned branch, a branch with a live
//!   handle — and then closes that set UPWARD through parents so the PARENT
//!   (transitively) of any retained branch is itself retained. It is
//!   time-INDEPENDENT (no `now`, no `now()`).
//! * [`reclaimable`] is the PURE policy that returns the dead set: branches
//!   neither structurally reachable NOR within the age-grace window, that are
//!   dead (explicitly dropped, or aged past `branch_grace`). The grace window
//!   is the fifth, time-DEPENDENT reachability root in the design's root list —
//!   it is realised here because it needs `now`.
//! * [`sweep`] / [`reclaim_branches`] are the only IO: they delete a dead
//!   branch's OWN divergent tail (`branches/<id>/world-events.jsonl`) and its
//!   branch-local snapshots (`branches/<id>/snapshots/`). They NEVER delete a
//!   pinned / live-handle / parent-of-retained branch, and NEVER touch the
//!   shared parent prefix — the MAIN log is the live session, always reachable.
//! * [`pin`] / [`drop_branch`] are the reachability-state mutators. Persistence
//!   is APPEND-ONLY latest-line-wins: each mutation appends a fresh
//!   `BranchDescriptor` line to `branches/index.jsonl` with the updated
//!   `pin`/`tombstone`, and [`load_latest_descriptors`] collapses the index by
//!   id keeping the LAST line for each. This honours the append-only JSONL
//!   discipline of the rest of the log (no in-place rewrite).
//!
//! Reclamation deletes only regenerable (snapshots — see `retention.rs`) or
//! unreachable (dead-branch tail) artifacts; the log is tiered, never deleted
//! (see `segment.rs`); nothing reachable from a root is ever removed
//! (Invariant 20).
//!
//! See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch pruning and
//! reachability GC (Invariant 20).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::agent::world::blob::{reachable_blobs, reclaimable_blobs, sweep_blobs, BlobStore};
use crate::agent::world::branch::{BranchDescriptor, BranchId, BranchStore};
use crate::agent::world::history::BlobHash;
use crate::agent::world::inputs::Event;
use crate::agent::world::world::{Tick, Timestamp, World};
use crate::error::Error;

// ---------------------------------------------------------------------------
// Roots — the reachability seed set
// ---------------------------------------------------------------------------

/// The reachability roots a branch sweep must protect.
///
/// `live_session` is the recorded MAIN log ([`BranchId::MAIN`] in the common
/// case): the live session is always reachable, so the shared parent prefix it
/// owns is never touched. `pinned` carries the ids a `pin` has marked as
/// reachability roots (in addition to any descriptor whose own `pin` flag is
/// set). `live_handles` carries the ids of branches with an open in-memory
/// handle (e.g. a replay session currently reading the branch). `grace` is the
/// age-grace window from `Caps.branch_grace`: a branch younger than `grace` is
/// retained (a time-dependent root applied by [`reclaimable`]); `0` means
/// unbounded — age alone never makes a branch dead (only an explicit drop does).
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// The live session — the recorded MAIN log. Always reachable.
    pub live_session: BranchId,
    /// Branch ids explicitly pinned as reachability roots.
    pub pinned: BTreeSet<BranchId>,
    /// Branch ids with a live in-memory handle.
    pub live_handles: BTreeSet<BranchId>,
    /// The age-grace window (`Caps.branch_grace`). A branch younger than this is
    /// retained. `0` ⇒ unbounded (age never reclaims; only a drop does).
    pub grace: Tick,
}

impl Default for Roots {
    /// The live session is the recorded MAIN log; no pins, no live handles, and
    /// an unbounded grace window (only explicit drops reclaim).
    fn default() -> Self {
        Roots {
            live_session: BranchId::MAIN,
            pinned: BTreeSet::new(),
            live_handles: BTreeSet::new(),
            grace: 0,
        }
    }
}

impl Roots {
    /// Roots for a session whose live branch is `live_session`, with the given
    /// grace window and no pins / live handles.
    pub fn new(live_session: BranchId, grace: Tick) -> Self {
        Roots {
            live_session,
            pinned: BTreeSet::new(),
            live_handles: BTreeSet::new(),
            grace,
        }
    }
}

// ---------------------------------------------------------------------------
// reachable — pure structural reachability (live/pin/handle + parent closure)
// ---------------------------------------------------------------------------

/// Mark every branch structurally reachable from a root.
///
/// The seed roots are: the live session (`roots.live_session`), every pinned
/// branch (`roots.pinned` UNION any descriptor whose own `pin` flag is set),
/// and every branch with a live handle (`roots.live_handles`). The seed set is
/// then closed UPWARD through parents: for each reachable branch, its parent —
/// and the parent's parent, transitively — is also reachable, so the PARENT of
/// any retained branch (and the shared prefix it owns) is itself retained. The
/// walk terminates at a parent that is not itself a listed branch (e.g.
/// [`BranchId::MAIN`], the root log) and on any cycle (an id is visited at most
/// once).
///
/// This function is PURE and time-INDEPENDENT: it takes no `now` and calls no
/// `now()`. The age-grace window — the fifth reachability root in the design's
/// root list — is time-dependent and is applied by [`reclaimable`].
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC
/// (Invariant 20).
pub fn reachable(branches: &[BranchDescriptor], roots: &Roots) -> BTreeSet<BranchId> {
    // Each branch's immediate parent, for the upward walk. MAIN (and any id not
    // materialised as a branch) is absent, which terminates the walk.
    let parent_of: BTreeMap<&BranchId, &BranchId> =
        branches.iter().map(|b| (&b.id, &b.parent)).collect();

    // Seed the structural roots. A descriptor's own `pin` flag and the runtime
    // `roots.pinned` set are both honoured, so a pin persisted to the index and
    // a pin supplied in-memory are equivalent.
    let mut seeds: BTreeSet<BranchId> = BTreeSet::new();
    seeds.insert(roots.live_session.clone());
    seeds.extend(roots.pinned.iter().cloned());
    seeds.extend(roots.live_handles.iter().cloned());
    for b in branches {
        if b.pin {
            seeds.insert(b.id.clone());
        }
    }

    // Close upward through parents. `reached.insert` returns `false` once an id
    // is already present, which both prevents revisiting an already-closed
    // ancestor chain and guards against a (malformed) parent cycle.
    let mut reached: BTreeSet<BranchId> = BTreeSet::new();
    for seed in seeds {
        let mut cur = seed;
        while reached.insert(cur.clone()) {
            match parent_of.get(&cur) {
                Some(parent) => cur = (*parent).clone(),
                None => break,
            }
        }
    }
    reached
}

// ---------------------------------------------------------------------------
// reclaimable — pure dead-set policy (unreachable AND dead)
// ---------------------------------------------------------------------------

/// Return the dead set: every branch that is safe to reclaim.
///
/// A branch is reclaimable iff it is BOTH:
///
/// 1. NOT structurally reachable ([`reachable`]) — i.e. not the live session,
///    not pinned, no live handle, and not the parent (transitively) of a
///    retained branch; AND
/// 2. DEAD — either explicitly dropped (`tombstone == true`) or aged past the
///    grace window (`roots.grace != 0` and `now - created_wall >= grace`).
///
/// A branch younger than the grace window (and not dropped) is therefore
/// RETAINED — the time-dependent grace root. `roots.grace == 0` disables
/// age-based death entirely (unbounded grace): only an explicit drop reclaims.
///
/// This function is PURE: `now` is passed in (no `now()` in the core), so the
/// same inputs always yield the same dead set.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC
/// (Invariant 20).
pub fn reclaimable(
    branches: &[BranchDescriptor],
    roots: &Roots,
    now: Timestamp,
) -> Vec<BranchId> {
    let reached = reachable(branches, roots);
    branches
        .iter()
        .filter(|b| !reached.contains(&b.id))
        .filter(|b| is_dead(b, roots.grace, now))
        .map(|b| b.id.clone())
        .collect()
}

/// Whether a branch is DEAD: explicitly dropped, or aged past the grace window.
///
/// An explicit drop (`tombstone`) is dead immediately — the user's intent is a
/// stronger signal than age. Otherwise the branch is dead once its age
/// (`now - created_wall`) reaches the grace window. `grace == 0` means the age
/// test never fires (unbounded grace), so a non-dropped branch is never dead by
/// age. Negative age (clock skew, `now < created_wall`) is treated as young.
fn is_dead(b: &BranchDescriptor, grace: Tick, now: Timestamp) -> bool {
    if b.tombstone {
        return true;
    }
    if grace == 0 {
        return false;
    }
    let age = now.saturating_sub(b.created_wall);
    let window = i64::try_from(grace).unwrap_or(i64::MAX);
    age >= window
}

// ---------------------------------------------------------------------------
// sweep — thin IO shell: delete each dead branch's own tail + snapshots
// ---------------------------------------------------------------------------

/// Delete each dead branch's OWN artifacts — its divergent tail
/// (`branches/<id>/world-events.jsonl`) and its branch-local snapshots
/// (`branches/<id>/snapshots/`), i.e. the whole `branches/<id>/` directory.
///
/// Returns the ids whose directory was actually removed (a branch already
/// reclaimed on a prior sweep is skipped, so reporting stays idempotent).
///
/// SAFETY: [`BranchId::MAIN`] is the empty string, and `branches/.join("")`
/// resolves to the `branches/` directory itself; sweeping it would destroy the
/// index and every other branch. MAIN can never appear in a [`reclaimable`]
/// result (it is the live session, always reachable), but this shell guards the
/// empty id defensively regardless. The shared parent prefix — the MAIN log
/// under `<session_dir>/world-events.jsonl` — is outside `branches/` and is
/// never touched.
///
/// Callers should only pass ids returned by [`reclaimable`].
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC.
pub fn sweep(session_dir: &Path, dead: &[BranchId]) -> Result<Vec<BranchId>, Error> {
    let branches_dir = session_dir.join("branches");
    let mut swept = Vec::new();
    for id in dead {
        // Never resolve an empty id (MAIN) to the branches/ directory itself.
        if id.0.is_empty() {
            continue;
        }
        let dir = branches_dir.join(&id.0);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
            swept.push(id.clone());
        }
    }
    Ok(swept)
}

/// The composed reclaim pass: load the latest branch descriptors, compute the
/// dead set under `roots`/`now`, and sweep it.
///
/// This is the seam the prune CLI (`CR-cli-prune`) wraps — pure policy
/// ([`reclaimable`]) then the per-branch delete ([`sweep`]). Returns the ids
/// whose artifacts were actually removed.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC.
pub fn reclaim_branches(
    session_dir: &Path,
    roots: &Roots,
    now: Timestamp,
) -> Result<Vec<BranchId>, Error> {
    let branches = load_latest_descriptors(session_dir)?;
    let dead = reclaimable(&branches, roots, now);
    sweep(session_dir, &dead)
}

// ---------------------------------------------------------------------------
// reclaim_blobs — the blob reachability roots, wired alongside dead-branch GC
// ---------------------------------------------------------------------------

/// The composed blob-reclaim pass: scan the RETAINED log segments + snapshots for
/// reachable blob hashes, subtract them from the store's stored set, and sweep the
/// remainder — the blob-store counterpart of [`reclaim_branches`].
///
/// `segments` are the retained log events (the live hot + cold segments the
/// segmented log still holds — `SegmentedEventLog::load`); `snapshots` are the
/// retained snapshot Worlds. A blob referenced by ANY of them is reachable and is
/// NEVER swept; the rest (`stored − reachable`) are reclaimed. Because the inputs
/// are the RETAINED data, a blob referenced only by a pruned/cold-dropped segment
/// is correctly reclaimable — reachability tracks live data, not history.
///
/// This is the seam a blob-prune caller (BL-sink) wraps — the pure predicate
/// ([`reachable_blobs`]/[`reclaimable_blobs`], in `blob.rs`) then the per-blob
/// delete ([`sweep_blobs`]). Returns the hashes actually removed. The only IO is
/// the store enumeration + the sweep; the reachability scan is pure.
///
/// See docs/agent/world/ecs-runtime.md §1426-1428 (GC by log-reachability) and
/// [`reclaim_branches`] (the dead-branch reachability GC this mirrors).
pub fn reclaim_blobs(
    store: &BlobStore,
    segments: &[Event],
    snapshots: &[World],
) -> Result<Vec<BlobHash>, Error> {
    let reachable = reachable_blobs(segments, snapshots);
    let stored = store.stored_hashes()?;
    let reclaimable = reclaimable_blobs(&stored, &reachable);
    sweep_blobs(store, &reclaimable, &reachable)
}

// ---------------------------------------------------------------------------
// pin / drop — reachability-state mutators (append-only latest-line-wins)
// ---------------------------------------------------------------------------

/// Set (or clear) a branch's `pin` reachability flag.
///
/// A pinned branch is a reachability root that dead-branch GC never reclaims.
/// Persistence is append-only: a fresh `BranchDescriptor` line carrying the
/// updated flag is appended to `branches/index.jsonl`, and
/// [`load_latest_descriptors`] resolves the branch's state to that LAST line.
///
/// Errors if no branch with `id` exists in the index.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC.
pub fn pin(session_dir: &Path, id: &BranchId, pinned: bool) -> Result<(), Error> {
    mutate_descriptor(session_dir, id, |d| d.pin = pinned)
}

/// Mark a branch as dropped (`tombstone = true`) — eligible for reclamation by
/// the next prune pass. Persistence is the same append-only latest-line-wins
/// discipline as [`pin`].
///
/// Errors if no branch with `id` exists in the index.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC.
pub fn drop_branch(session_dir: &Path, id: &BranchId) -> Result<(), Error> {
    mutate_descriptor(session_dir, id, |d| d.tombstone = true)
}

/// Read the latest descriptor for `id`, apply `f`, and append the result.
fn mutate_descriptor(
    session_dir: &Path,
    id: &BranchId,
    f: impl FnOnce(&mut BranchDescriptor),
) -> Result<(), Error> {
    let mut current = load_latest_descriptors(session_dir)?
        .into_iter()
        .find(|d| &d.id == id)
        .ok_or_else(|| {
            Error::World(format!("no branch with id {} to mutate", id.0))
        })?;
    f(&mut current);
    append_descriptor(&index_path(session_dir), &current)
}

/// Load the branch descriptors with append-only latest-line-wins resolution:
/// the index may carry multiple lines per id (creation, then each mutation);
/// the LAST line for each id is its current state. Order follows first
/// appearance of each id, matching the creation order [`BranchStore::list`]
/// preserves.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC.
pub fn load_latest_descriptors(session_dir: &Path) -> Result<Vec<BranchDescriptor>, Error> {
    let all = BranchStore::new(session_dir).list()?;
    let mut order: Vec<BranchId> = Vec::new();
    let mut latest: BTreeMap<BranchId, BranchDescriptor> = BTreeMap::new();
    for d in all {
        if !latest.contains_key(&d.id) {
            order.push(d.id.clone());
        }
        latest.insert(d.id.clone(), d);
    }
    Ok(order
        .into_iter()
        .filter_map(|id| latest.get(&id).cloned())
        .collect())
}

/// The branch index path: `<session_dir>/branches/index.jsonl`.
fn index_path(session_dir: &Path) -> std::path::PathBuf {
    session_dir.join("branches").join("index.jsonl")
}

/// Append one `BranchDescriptor` as a JSON line, mirroring the append-only JSONL
/// discipline of `branch::persist_branch` (create parents, open append, write
/// one line). Durable before the call returns.
fn append_descriptor(path: &Path, descriptor: &BranchDescriptor) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let json = serde_json::to_string(descriptor)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::world::branch::{fork, persist_branch, Edit, EditKind};
    use crate::agent::world::inputs::{Event, Fingerprint, LogicalInput, Origin};

    // --- pure fixtures -----------------------------------------------------

    fn desc(
        id: &str,
        parent: BranchId,
        pin: bool,
        tombstone: bool,
        created_wall: Timestamp,
    ) -> BranchDescriptor {
        BranchDescriptor {
            id: BranchId(id.into()),
            parent,
            fork_tick: 1,
            edit_summary: String::new(),
            created_wall,
            prefix_digest: Fingerprint(String::new()),
            pin,
            tombstone,
        }
    }

    // -----------------------------------------------------------------------
    // reachable — live session, pin, live handle, parent closure
    // -----------------------------------------------------------------------

    /// The live session (MAIN) is always reachable, protecting the shared prefix.
    #[test]
    fn live_session_is_always_reachable() {
        let branches: Vec<BranchDescriptor> = Vec::new();
        let reached = reachable(&branches, &Roots::default());
        assert!(
            reached.contains(&BranchId::MAIN),
            "the live session MAIN must always be reachable"
        );
    }

    /// A branch whose own `pin` flag is set is reachable; one supplied via
    /// `roots.pinned` is equally reachable.
    #[test]
    fn pinned_branch_is_reachable_via_flag_and_via_roots() {
        let by_flag = desc("a", BranchId::MAIN, true, false, 0);
        let by_root = desc("b", BranchId::MAIN, false, false, 0);
        let branches = vec![by_flag, by_root];

        let mut roots = Roots::default();
        roots.pinned.insert(BranchId("b".into()));

        let reached = reachable(&branches, &roots);
        assert!(reached.contains(&BranchId("a".into())), "descriptor.pin is a root");
        assert!(reached.contains(&BranchId("b".into())), "roots.pinned is a root");
    }

    /// A branch with a live handle is reachable.
    #[test]
    fn live_handle_branch_is_reachable() {
        let b = desc("a", BranchId::MAIN, false, false, 0);
        let branches = vec![b];
        let mut roots = Roots::default();
        roots.live_handles.insert(BranchId("a".into()));
        let reached = reachable(&branches, &roots);
        assert!(reached.contains(&BranchId("a".into())), "a live-handle branch is reachable");
    }

    /// The PARENT (transitively) of a retained branch is reachable, even when the
    /// parent is itself tombstoned — the upward closure protects the lineage and
    /// the shared prefix.
    #[test]
    fn parent_of_retained_branch_is_reachable() {
        // MAIN <- b (tombstoned) <- c (pinned).
        let b = desc("b", BranchId::MAIN, false, true, 0);
        let c = desc("c", BranchId("b".into()), true, false, 0);
        let branches = vec![b, c];

        let reached = reachable(&branches, &Roots::default());
        assert!(reached.contains(&BranchId("c".into())), "pinned child is reachable");
        assert!(
            reached.contains(&BranchId("b".into())),
            "the parent of a retained branch must be reachable even when tombstoned"
        );
        assert!(reached.contains(&BranchId::MAIN), "the grandparent MAIN is reachable");
    }

    // -----------------------------------------------------------------------
    // reclaimable — dropped / aged reclaimed; reachable / young retained
    // -----------------------------------------------------------------------

    /// A dropped (tombstoned), unreferenced branch is reclaimed.
    #[test]
    fn dropped_unreferenced_branch_is_reclaimed() {
        let dead = desc("a", BranchId::MAIN, false, true, 0);
        let branches = vec![dead];
        let got = reclaimable(&branches, &Roots::default(), 1_000);
        assert_eq!(got, vec![BranchId("a".into())], "a dropped unreferenced branch is reclaimed");
    }

    /// An aged (past the grace window), unreferenced, non-dropped branch is
    /// reclaimed.
    #[test]
    fn aged_unreferenced_branch_is_reclaimed() {
        let old = desc("a", BranchId::MAIN, false, false, 100);
        let branches = vec![old];
        let roots = Roots::new(BranchId::MAIN, 50); // grace window = 50
        // now = 200 ⇒ age = 100 ≥ 50 ⇒ aged out.
        let got = reclaimable(&branches, &roots, 200);
        assert_eq!(got, vec![BranchId("a".into())], "an aged unreferenced branch is reclaimed");
    }

    /// A branch younger than the grace window is RETAINED (the grace root), even
    /// though it is unreferenced and not pinned.
    #[test]
    fn young_unreferenced_branch_is_within_grace() {
        let young = desc("a", BranchId::MAIN, false, false, 180);
        let branches = vec![young];
        let roots = Roots::new(BranchId::MAIN, 50);
        // now = 200 ⇒ age = 20 < 50 ⇒ within grace.
        let got = reclaimable(&branches, &roots, 200);
        assert!(got.is_empty(), "a branch within the grace window must be retained");
    }

    /// `grace == 0` (unbounded) disables age-based reclamation: an old,
    /// unreferenced, non-dropped branch is retained; only an explicit drop
    /// reclaims.
    #[test]
    fn grace_zero_reclaims_only_dropped() {
        let aged = desc("a", BranchId::MAIN, false, false, 0);
        let dropped = desc("b", BranchId::MAIN, false, true, 0);
        let branches = vec![aged, dropped];
        let got = reclaimable(&branches, &Roots::default(), 1_000_000);
        assert_eq!(
            got,
            vec![BranchId("b".into())],
            "with grace=0 only the dropped branch is reclaimed; the aged one is retained"
        );
    }

    /// A PINNED branch is retained — never in the dead set.
    #[test]
    fn pinned_branch_is_retained() {
        // Pinned AND tombstoned AND old: pin still wins.
        let pinned = desc("a", BranchId::MAIN, true, true, 0);
        let branches = vec![pinned];
        let roots = Roots::new(BranchId::MAIN, 1);
        let got = reclaimable(&branches, &roots, 1_000_000);
        assert!(got.is_empty(), "a pinned branch must be retained regardless of age/drop");
    }

    /// A reachable branch's files (and the shared prefix) are never in the dead
    /// set: a tombstoned parent of a pinned child is retained.
    #[test]
    fn reachable_parent_of_pinned_child_is_never_reclaimed() {
        let parent = desc("b", BranchId::MAIN, false, true, 0); // dropped…
        let child = desc("c", BranchId("b".into()), true, false, 0); // …but pinned child
        let branches = vec![parent, child];
        let got = reclaimable(&branches, &Roots::new(BranchId::MAIN, 1), 1_000_000);
        assert!(
            got.is_empty(),
            "neither the pinned child nor its (reachable) parent may be reclaimed"
        );
    }

    // -----------------------------------------------------------------------
    // IO: sweep removes ONLY the dead branch's own tail + snapshots
    // -----------------------------------------------------------------------

    fn session_started() -> LogicalInput {
        LogicalInput::SessionStarted {
            seed: 0xABCD,
            surface_tools: Vec::new(),
        }
    }

    fn user_message(text: &str) -> LogicalInput {
        LogicalInput::UserMessage { to: 0, text: text.into() }
    }

    fn ev(at: Tick, origin: Origin, input: LogicalInput) -> Event {
        Event { origin, edge: 0, at, wall: None, input }
    }

    fn sample_log() -> Vec<Event> {
        vec![
            ev(0, Origin::System, session_started()),
            ev(1, Origin::Human, user_message("hello")),
            ev(2, Origin::Human, user_message("again")),
        ]
    }

    /// Persist a branch via the real `fork` + `persist_branch` path, then plant a
    /// branch-local snapshot file under `branches/<id>/snapshots/` so the sweep's
    /// removal of branch-local snapshots is observable.
    fn persist_with_snapshot(session_dir: &Path, text: &str) -> BranchId {
        let log = sample_log();
        let edit = Edit { at_tick: 1, input: user_message(text), kind: EditKind::Replace };
        let (id, branch) = fork(&log, 1, edit.clone()).expect("fork");
        persist_branch(session_dir, &BranchId::MAIN, 1, &edit, &branch, 1_000)
            .expect("persist branch");
        let snap_dir = session_dir.join("branches").join(&id.0).join("snapshots");
        std::fs::create_dir_all(&snap_dir).expect("create branch snapshots dir");
        std::fs::write(snap_dir.join("1.snapshot.json"), b"{}").expect("write branch snapshot");
        id
    }

    /// The sweep deletes only the dead branch's own tail + branch-local
    /// snapshots; a reachable (pinned) branch, the shared parent prefix (the
    /// MAIN log), and the index are all left intact.
    #[test]
    fn sweep_removes_only_the_dead_branch_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();

        // The shared parent prefix — the MAIN log — must never be touched.
        std::fs::create_dir_all(session_dir).expect("create session dir");
        std::fs::write(session_dir.join("world-events.jsonl"), b"main\n").expect("write MAIN log");

        let dead_id = persist_with_snapshot(session_dir, "dead-branch");
        let live_id = persist_with_snapshot(session_dir, "live-branch");

        // Drop the dead one; pin the live one.
        drop_branch(session_dir, &dead_id).expect("drop dead");
        pin(session_dir, &live_id, true).expect("pin live");

        // Reclaim: only the dropped, unreferenced branch is dead.
        let swept = reclaim_branches(session_dir, &Roots::default(), 10_000).expect("reclaim");
        assert_eq!(swept, vec![dead_id.clone()], "only the dropped branch is swept");

        let branches = session_dir.join("branches");
        // Dead branch's own tail + snapshots are gone.
        assert!(
            !branches.join(&dead_id.0).exists(),
            "the dead branch's own directory (tail + snapshots) must be removed"
        );
        // The reachable (pinned) branch is intact, tail and snapshots both.
        assert!(
            branches.join(&live_id.0).join("world-events.jsonl").exists(),
            "the pinned branch's tail must be retained"
        );
        assert!(
            branches.join(&live_id.0).join("snapshots").join("1.snapshot.json").exists(),
            "the pinned branch's branch-local snapshot must be retained"
        );
        // The shared parent prefix (MAIN log) and the index survive untouched.
        assert_eq!(
            std::fs::read(session_dir.join("world-events.jsonl")).expect("read MAIN log"),
            b"main\n",
            "the shared parent prefix (MAIN log) must never be touched"
        );
        assert!(branches.join("index.jsonl").exists(), "the branch index must survive");
    }

    /// `sweep` never resolves the empty MAIN id to the `branches/` directory
    /// itself: passing MAIN leaves every branch and the index intact.
    #[test]
    fn sweep_never_touches_branches_dir_for_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();
        let id = persist_with_snapshot(session_dir, "keep-me");

        let swept = sweep(session_dir, &[BranchId::MAIN]).expect("sweep MAIN");
        assert!(swept.is_empty(), "MAIN is never swept");
        assert!(
            session_dir.join("branches").join(&id.0).exists(),
            "sweeping MAIN must not delete the branches/ directory or its contents"
        );
        assert!(session_dir.join("branches").join("index.jsonl").exists());
    }

    // -----------------------------------------------------------------------
    // IO: pin / drop persist via append-only latest-line-wins
    // -----------------------------------------------------------------------

    /// `pin` and `drop_branch` persist via appended lines, and
    /// `load_latest_descriptors` resolves the branch to its LAST line.
    #[test]
    fn pin_and_drop_persist_latest_line_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();
        let id = persist_with_snapshot(session_dir, "branch");

        // Initially neither pinned nor tombstoned.
        let before = load_latest_descriptors(session_dir).expect("load");
        let b0 = before.iter().find(|d| d.id == id).expect("descriptor present");
        assert!(!b0.pin && !b0.tombstone, "fresh branch is neither pinned nor dropped");

        pin(session_dir, &id, true).expect("pin");
        let after_pin = load_latest_descriptors(session_dir).expect("load after pin");
        let b1 = after_pin.iter().find(|d| d.id == id).expect("descriptor present");
        assert!(b1.pin, "pin must persist (latest line wins)");

        drop_branch(session_dir, &id).expect("drop");
        let after_drop = load_latest_descriptors(session_dir).expect("load after drop");
        let b2 = after_drop.iter().find(|d| d.id == id).expect("descriptor present");
        assert!(b2.tombstone, "drop must persist the tombstone (latest line wins)");
        assert!(b2.pin, "the prior pin is carried forward into the latest line");

        // Exactly one descriptor per id despite multiple appended lines.
        assert_eq!(
            after_drop.iter().filter(|d| d.id == id).count(),
            1,
            "latest-line-wins collapses the id to a single descriptor"
        );
    }

    /// Mutating an unknown branch id errors rather than silently appending.
    #[test]
    fn mutate_unknown_branch_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = pin(dir.path(), &BranchId("does-not-exist".into()), true);
        assert!(result.is_err(), "pinning an unknown branch must error");
    }

    // -----------------------------------------------------------------------
    // reclaim_blobs — composed scan + sweep over retained segments/snapshots
    // -----------------------------------------------------------------------

    /// The composed blob pass keeps a blob referenced by a retained segment and
    /// sweeps one referenced by nothing — the blob-store mirror of the dead-branch
    /// sweep, wired here alongside it (the predicate's own keep/reclaim/non-vacuity
    /// proofs live in `blob.rs`).
    #[test]
    fn reclaim_blobs_keeps_referenced_sweeps_orphan() {
        use crate::agent::world::history::{Block, ImageSource};

        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let referenced = store.put(b"referenced blob").expect("put referenced");
        let orphan = store.put(b"orphan blob").expect("put orphan");

        // A retained log event references only `referenced`.
        let segment = Event {
            origin: Origin::Agent,
            edge: 0,
            at: 1,
            wall: None,
            input: LogicalInput::ToolReturned {
                cmd: 1,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                result: vec![Block::Image {
                    source: ImageSource::Blob {
                        hash: referenced.clone(),
                        mime: "image/png".into(),
                    },
                }],
            },
        };

        let swept = reclaim_blobs(&store, &[segment], &[]).expect("reclaim_blobs");
        assert_eq!(swept, vec![orphan.clone()], "only the unreferenced blob is swept");
        assert!(store.get(&orphan).is_err(), "the orphan blob is deleted");
        assert_eq!(
            store.get(&referenced).expect("get referenced"),
            b"referenced blob",
            "the referenced blob survives the composed pass"
        );
    }
}
