// These items are the branch identity + materialization layer: the
// content-addressed `BranchId`, the `Edit` type, the exogenous validator, the
// `BranchDescriptor` envelope, the pure `fork`, and the `persist_branch` IO
// seam. They are consumed by branch-replay (CR-branch-replay), the branch store
// (CR-branch-store), reclaim (CR-reclaim), and the CLI (CR-cli-eval); the binary
// target has no caller yet, so silence dead-code lints here.
#![allow(dead_code)]

//! Branch identity and materialization: content-addressed `BranchId`, the
//! `Edit` type, the exogenous-edit validator, the `BranchDescriptor` envelope,
//! the pure [`fork`], and the [`persist_branch`] storage seam.
//!
//! A `BranchId` is a deterministic content address: the same parent, fork
//! tick, and edit always produce the same id; distinct edits produce distinct
//! ids (collision-resistant under SHA-256). The root (recorded) log is
//! identified by the reserved constant [`BranchId::MAIN`] — an empty string,
//! which is unambiguous because every computed id is a 64-char hex digest.
//!
//! Only EXOGENOUS inputs may be edited in place (see [`edit_is_exogenous`]).
//! Derived model/tool results are never edited — to change one, rewind and
//! re-run live (fork at the relevant tick and let the branch replay go live).
//!
//! [`fork`] materializes a counterfactual branch as a PURE function: it takes
//! the verbatim prefix `[0..at_tick)` by clone, applies one exogenous edit at
//! the fork point, and re-stamps the tail monotonically — returning the full
//! branch log ready for `replay::replay_branch`. [`persist_branch`] is the
//! separate IO seam that writes that log under the session's `branches/`
//! directory and records a `BranchDescriptor` line.
//!
//! See docs/agent/world/ecs-runtime.md — Branch identity and storage layout;
//! FORK/EDIT algorithm (edit-scope, content-addressed replay).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::world::event_log::{EventLog, FilesystemEventLog};
use crate::agent::world::inputs::{Event, Fingerprint, LogicalInput, Origin};
use crate::agent::world::replay::is_exogenous;
use crate::agent::world::world::{Tick, Timestamp, World};
use crate::error::Error;

// ---------------------------------------------------------------------------
// BranchId — content-addressed branch identity
// ---------------------------------------------------------------------------

/// A content-addressed branch identity: `hex(SHA-256(parent_ref ‖
/// fork_tick.to_le_bytes() ‖ canonical(edit)))`, where `canonical(edit)` is
/// a fixed-order `serde_json::to_vec` of the [`Edit`]. The same parent, fork
/// tick, and edit always map to the same id; distinct edits produce distinct
/// ids.
///
/// The reserved constant [`BranchId::MAIN`] (empty string) identifies the root
/// (recorded) log. Every computed id is a 64-char hex digest, so the empty
/// string is unambiguous.
///
/// See docs/agent/world/ecs-runtime.md — Branch identity and storage layout.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BranchId(pub String);

impl BranchId {
    /// The reserved identity of the root (recorded) log. An empty string —
    /// unambiguous because every computed `BranchId` is a 64-char hex digest.
    pub const MAIN: Self = Self(String::new());

    /// Compute the content-addressed identity for a new branch.
    ///
    /// Feeds `SHA-256` with three concatenated byte sequences in fixed order:
    /// 1. `parent.0.as_bytes()` — the parent branch identity (or MAIN → `b""`).
    /// 2. `fork_tick.to_le_bytes()` — the fork tick as little-endian u64.
    /// 3. `serde_json::to_vec(edit)` — the canonical `Edit` serialization.
    ///
    /// The output is `hex::encode(digest)` — the same encoding used by
    /// `effects::fingerprint_call`. Identical inputs → identical id; distinct
    /// edits → distinct ids (SHA-256 collision-resistant).
    pub fn compute(parent: &BranchId, fork_tick: Tick, edit: &Edit) -> Result<Self, Error> {
        let canonical = serde_json::to_vec(edit)?;
        let mut hasher = Sha256::new();
        hasher.update(parent.0.as_bytes());
        hasher.update(fork_tick.to_le_bytes());
        hasher.update(&canonical);
        Ok(BranchId(hex::encode(hasher.finalize())))
    }
}

// ---------------------------------------------------------------------------
// EditKind — replace the exogenous input or extend the session with a new one
// ---------------------------------------------------------------------------

/// Whether the edit replaces the existing input at the given tick or extends
/// the session by appending a new input after the fork tick.
///
/// `Replace` is the common case for counterfactual evaluation: swap one
/// exogenous input for another and replay. `Extend` is for appending a new
/// exogenous turn (e.g. a follow-up user message) after a fork point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditKind {
    /// Swap the exogenous input at `Edit::at_tick` for `Edit::input`.
    Replace,
    /// Append `Edit::input` as a new event immediately after `Edit::at_tick`.
    Extend,
}

// ---------------------------------------------------------------------------
// Edit — one exogenous-input edit applied at a specific tick
// ---------------------------------------------------------------------------

/// A single edit applied to an exogenous input at a specific tick.
///
/// Only exogenous inputs (see [`edit_is_exogenous`]) may be the target of an
/// `Edit`. Derived model/tool results carry fingerprints that bind them to the
/// request that produced them; editing them in place is rejected — to change a
/// derived result, fork before it and let the branch go live at the divergence.
///
/// `canonical(edit) = serde_json::to_vec(edit)` is the deterministic byte
/// sequence hashed into [`BranchId::compute`].
///
/// See docs/agent/world/ecs-runtime.md — FORK/EDIT algorithm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    /// The tick the edit targets.
    pub at_tick: Tick,
    /// The replacement or extension input (must be exogenous — checked by
    /// [`edit_is_exogenous`] before any `BranchId` is computed).
    pub input: LogicalInput,
    /// Whether the edit replaces the event at `at_tick` or extends past it.
    pub kind: EditKind,
}

// ---------------------------------------------------------------------------
// edit_is_exogenous — the exogenous-edit validator (VC-1.4)
// ---------------------------------------------------------------------------

/// Whether `input` is a valid target for an [`Edit`].
///
/// Delegates to [`replay::is_exogenous`], which is the exact complement of
/// `effects::is_derived_result`: derived model/tool results (`ModelResponded`,
/// `ModelFailed`, `InferenceCancelled`, `ToolReturned`, `HumanActionDone`,
/// `Compacted`) return `false`; every other input — the free variables the
/// runtime drives the world with — returns `true`.
///
/// Callers MUST check this before computing a `BranchId` or materialising a
/// fork; `fork` (CR-fork) relies on this gate.
///
/// See docs/agent/world/ecs-runtime.md — FORK/EDIT algorithm; VC-1.4.
pub fn edit_is_exogenous(input: &LogicalInput) -> bool {
    is_exogenous(input)
}

// ---------------------------------------------------------------------------
// BranchDescriptor — the branch metadata record
// ---------------------------------------------------------------------------

/// Persistent metadata for one branch, stored in `branches/index.jsonl`
/// (one record per line).
///
/// `id` uniquely identifies the branch (content-addressed). `parent`
/// identifies its immediate ancestor — [`BranchId::MAIN`] for a first-level
/// fork off the root log. `prefix_digest` is the `Fingerprint` of the shared
/// prefix `[0..fork_tick)` so downstream tooling can assert the prefix is
/// intact without re-folding the log. `created_wall` is the wall-clock
/// timestamp supplied by the caller at fork time (not resolved here — no
/// `now()` in pure constructors). `pin` keeps the branch reachable through
/// GC; `tombstone` marks it as dropped.
///
/// See docs/agent/world/ecs-runtime.md — Branch identity and storage layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchDescriptor {
    /// The content-addressed identity of this branch.
    pub id: BranchId,
    /// The parent branch (`BranchId::MAIN` for a first-level fork).
    pub parent: BranchId,
    /// The tick at which the fork was made (the edit's target tick).
    pub fork_tick: Tick,
    /// A human-readable summary of the edit (e.g. the edited input text).
    pub edit_summary: String,
    /// The wall-clock timestamp at which the branch was created, supplied by
    /// the caller (not resolved inside pure constructors).
    pub created_wall: Timestamp,
    /// The SHA-256 digest of the shared prefix `[0..fork_tick)` so
    /// downstream tooling can verify the prefix without re-folding.
    pub prefix_digest: Fingerprint,
    /// When `true`, this branch is a reachability root and is never reclaimed
    /// by dead-branch GC.
    pub pin: bool,
    /// When `true`, the branch has been explicitly dropped and is eligible for
    /// reclamation by the next `prune` pass.
    pub tombstone: bool,
}

// ---------------------------------------------------------------------------
// fork — pure branch materialization (prefix-by-clone + one exogenous edit)
// ---------------------------------------------------------------------------

/// Materialize a counterfactual branch from a recorded log.
///
/// `fork` is PURE — no IO, no `now()`. It:
///
/// 1. Validates the edit targets an EXOGENOUS input via [`edit_is_exogenous`]
///    (VC-1.4): a derived model/tool result is rejected with `Error::World`.
/// 2. Takes the verbatim prefix `[0..at_tick)` — every event with `at < at_tick`
///    cloned byte-for-byte, INCLUDING the derived results, so on replay the
///    prefix re-fingerprints and reuses them (zero model calls).
/// 3. Applies the edit at the fork point: [`EditKind::Replace`] substitutes the
///    exogenous input at `at_tick`; [`EditKind::Extend`] keeps the original
///    input at `at_tick` and inserts the new one immediately after it.
/// 4. Re-stamps the tail monotonically from `at_tick`, so the whole branch log
///    has strictly increasing ticks while the prefix keeps its original ticks.
///
/// The `original_events` slice is taken by shared reference and never mutated.
/// The returned `Vec<Event>` is the full materialized branch log
/// (verbatim prefix ++ edited tail), ready to feed `replay::replay_branch`. The
/// returned [`BranchId`] forks off the root recorded log
/// ([`BranchId::MAIN`]); identical `(at_tick, edit)` always yield the same id.
///
/// See docs/agent/world/ecs-runtime.md — FORK/EDIT algorithm; VC-1.1.
pub fn fork(
    original_events: &[Event],
    at_tick: Tick,
    edit: Edit,
) -> Result<(BranchId, Vec<Event>), Error> {
    // (1) Edit-scope gate: only exogenous inputs may be edited in place.
    if !edit_is_exogenous(&edit.input) {
        return Err(Error::World(
            "fork edit must target an exogenous input; a derived model/tool \
             result is never edited in place — rewind and re-run live instead"
                .into(),
        ));
    }

    // (1b) For Replace: also validate that the ORIGINAL slot at `at_tick` is
    // exogenous. A Replace cannot edit a derived result in place — to change a
    // derived result, rewind past it and let the branch replay go live
    // (VC-1.4; docs/agent/world/ecs-runtime.md — FORK/EDIT algorithm).
    if edit.kind == EditKind::Replace {
        match original_events.iter().find(|e| e.at == at_tick) {
            None => {
                return Err(Error::World(format!(
                    "fork Replace edit targets tick {at_tick} but no event \
                     exists at that tick in the original log"
                )));
            }
            Some(slot) if !is_exogenous(&slot.input) => {
                return Err(Error::World(
                    "fork Replace edit cannot target a derived slot; the \
                     original input at `at_tick` is a derived model/tool \
                     result — rewind and re-run live instead"
                        .into(),
                ));
            }
            Some(_) => {}
        }
    }

    // (2) The verbatim prefix [0..at_tick): cloned byte-for-byte, carrying its
    // derived results so the branch re-fingerprints and reuses them on replay.
    let mut branch: Vec<Event> = original_events
        .iter()
        .filter(|e| e.at < at_tick)
        .cloned()
        .collect();

    // The synthetic edit event. `Replace` inherits the replaced slot's envelope
    // (same actor/edge/wall, swapped input); `Extend` is a brand-new turn whose
    // origin follows the input kind and whose edge follows the fork-point anchor.
    let anchor = original_events.iter().find(|e| e.at == at_tick);
    let edit_event = match edit.kind {
        EditKind::Replace => match anchor {
            Some(a) => Event {
                input: edit.input.clone(),
                ..a.clone()
            },
            None => Event {
                origin: origin_for_input(&edit.input),
                edge: 0,
                at: at_tick,
                wall: None,
                input: edit.input.clone(),
            },
        },
        EditKind::Extend => Event {
            origin: origin_for_input(&edit.input),
            edge: anchor.map_or(0, |a| a.edge),
            at: at_tick,
            wall: None,
            input: edit.input.clone(),
        },
    };

    // (3) Assemble the edited tail in order, then (4) re-stamp it monotonically.
    let mut tail: Vec<Event> = match edit.kind {
        EditKind::Replace => {
            // Drop the slot at `at_tick`; substitute the edit at the head.
            let mut t = vec![edit_event];
            t.extend(
                original_events
                    .iter()
                    .filter(|e| e.at > at_tick)
                    .cloned(),
            );
            t
        }
        EditKind::Extend => {
            // Keep the original slot(s) at `at_tick`, insert the edit after them.
            let mut t: Vec<Event> = original_events
                .iter()
                .filter(|e| e.at == at_tick)
                .cloned()
                .collect();
            t.push(edit_event);
            t.extend(
                original_events
                    .iter()
                    .filter(|e| e.at > at_tick)
                    .cloned(),
            );
            t
        }
    };
    restamp_from(&mut tail, at_tick);
    branch.extend(tail);

    // The branch forks off the root recorded log; the id is content-addressed
    // over (parent, fork_tick, canonical(edit)).
    let id = BranchId::compute(&BranchId::MAIN, at_tick, &edit)?;
    Ok((id, branch))
}

/// Re-stamp `events` with strictly increasing ticks starting at `from`, in their
/// current order. The fork prefix already occupies ticks below `from`, so the
/// whole branch is monotonic once the tail is stamped `from, from+1, …`.
fn restamp_from(events: &mut [Event], from: Tick) {
    for (offset, ev) in events.iter_mut().enumerate() {
        ev.at = from + offset as u64;
    }
}

/// The `Event.origin` to stamp on a synthetic exogenous edit, mirroring the
/// submission convention in `world::driver`: a user message or a human surface
/// mutation is a `Human`-origin free variable; everything else is `System`-neutral.
fn origin_for_input(input: &LogicalInput) -> Origin {
    match input {
        LogicalInput::UserMessage { .. } | LogicalInput::SurfaceMutated { .. } => Origin::Human,
        _ => Origin::System,
    }
}

// ---------------------------------------------------------------------------
// persist_branch — the IO seam (write branch log + append descriptor)
// ---------------------------------------------------------------------------

/// Persist a forked branch under a session directory, TAIL-ONLY.
///
/// This is the IO counterpart to the pure [`fork`]: it writes ONLY the branch's
/// divergent tail — the events at/after the fork tick (`e.at >= fork_tick`) — to
/// `<session_dir>/branches/<branch_id>/world-events.jsonl` via the same
/// [`FilesystemEventLog`] append discipline the live runtime uses, then appends
/// one [`BranchDescriptor`] line to `<session_dir>/branches/index.jsonl`. The
/// shared prefix `[0..fork_tick)` is NOT cloned to disk: it is resolved by
/// reference to the parent at load time ([`BranchStore::load_events`]). The
/// `BranchDescriptor`'s `parent` + `fork_tick` are the pointer to that prefix,
/// and `prefix_digest` validates it on load — so branch disk use is the tail
/// size, not a full prefix clone per branch.
///
/// The branch id is recomputed as `BranchId::compute(parent, fork_tick, edit)`,
/// so the on-disk directory name and the descriptor identity always agree with
/// what [`fork`] returned for the same inputs. `prefix_digest` is the digest of
/// the verbatim prefix `[0..fork_tick)` (taken here from the passed
/// `branch_events` head, byte-identical to the parent's prefix), so a reader can
/// assert the shared prefix is intact without re-folding the log. `created_wall`
/// is supplied by the IO caller — there is no `now()` in the pure core. The
/// written [`BranchDescriptor`] is returned for the caller to display.
///
/// See docs/agent/world/ecs-runtime.md — Tail-only on-disk storage (writing only
/// events >= fork_tick and resolving the prefix by a pointer to the parent).
pub fn persist_branch(
    session_dir: &Path,
    parent: &BranchId,
    fork_tick: Tick,
    edit: &Edit,
    branch_events: &[Event],
    created_wall: Timestamp,
) -> Result<BranchDescriptor, Error> {
    let id = BranchId::compute(parent, fork_tick, edit)?;

    // The verbatim prefix is the head of the branch log below the fork tick
    // (fork clones it unchanged), so digesting it equals digesting the original.
    let prefix: Vec<Event> = branch_events
        .iter()
        .filter(|e| e.at < fork_tick)
        .cloned()
        .collect();
    let prefix_digest = digest_events(&prefix)?;

    let descriptor = BranchDescriptor {
        id: id.clone(),
        parent: parent.clone(),
        fork_tick,
        edit_summary: summarize_edit(edit),
        created_wall,
        prefix_digest,
        pin: false,
        tombstone: false,
    };

    let branches_dir = session_dir.join("branches");
    let mut log =
        FilesystemEventLog::new(branches_dir.join(&id.0).join("world-events.jsonl"));
    // Tail-only: write only the events at/after the fork tick. The prefix
    // `[0..fork_tick)` is shared by reference to the parent (resolved on load),
    // so it is never cloned to disk.
    for ev in branch_events.iter().filter(|e| e.at >= fork_tick) {
        log.append(ev)?;
    }
    append_descriptor(&branches_dir.join("index.jsonl"), &descriptor)?;

    Ok(descriptor)
}

/// Digest a slice of events: `hex(SHA-256(serde_json::to_vec(events)))` — the
/// same content-addressing scheme as the snapshot/fingerprint digests, applied
/// here to the verbatim prefix so "the prefix is byte-identical to the original"
/// is provable from the digest alone.
fn digest_events(events: &[Event]) -> Result<Fingerprint, Error> {
    let bytes = serde_json::to_vec(events)?;
    Ok(Fingerprint(hex::encode(Sha256::digest(&bytes))))
}

/// A short, human-readable summary of an edit for the branch index.
fn summarize_edit(edit: &Edit) -> String {
    let verb = match edit.kind {
        EditKind::Replace => "replace",
        EditKind::Extend => "extend",
    };
    match &edit.input {
        LogicalInput::UserMessage { text, .. } => {
            format!("{verb}@{} user message: {text}", edit.at_tick)
        }
        _ => format!("{verb}@{} exogenous input", edit.at_tick),
    }
}

/// Append one [`BranchDescriptor`] as a JSON line to `index_path`, mirroring the
/// append-only JSONL discipline of `event_log` (create parents, open append,
/// write one line). A returned `Ok(())` is durable before the call returns.
fn append_descriptor(index_path: &Path, descriptor: &BranchDescriptor) -> Result<(), Error> {
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path)?;
    let json = serde_json::to_string(descriptor)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// BranchDiff — the result of comparing two branch Worlds
// ---------------------------------------------------------------------------

/// Summary of a branch-vs-original or branch-vs-branch comparison.
///
/// When `diverged` is `false`, both worlds are structurally identical (via
/// `PartialEq`) and `diverged_at` is `None`. This is the no-op-edit case
/// (VC-1.3): the branch World is byte-identical to the original at the same
/// tick.
///
/// When `diverged` is `true`, the worlds differ at or before the minimum of
/// their clocks; `diverged_at` is `Some(min(world_a.clock, world_b.clock))`.
/// Because the shared prefix is replayed byte-identically and only the
/// divergent tail produces a different World, the divergence is known to have
/// occurred at or before that conservative tick (VC-1.1).
///
/// `a_history_len` and `b_history_len` are the total conversation-message
/// counts across all entities in each world — a quick measure of how much each
/// branch produced before the comparison point.
///
/// See docs/agent/world/ecs-runtime.md — Branch identity and storage layout;
/// VC-1.1 (byte-identical prefix), VC-1.3 (no-op edit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchDiff {
    /// `true` when the two worlds differ structurally in at least one field.
    pub diverged: bool,
    /// The conservative earliest tick at which divergence is confirmed.
    /// `None` when the worlds are identical.
    pub diverged_at: Option<Tick>,
    /// Logical clock of `world_a` at comparison time.
    pub a_clock: Tick,
    /// Logical clock of `world_b` at comparison time.
    pub b_clock: Tick,
    /// Total conversation-message count (summed over all entities) in `world_a`.
    pub a_history_len: usize,
    /// Total conversation-message count (summed over all entities) in `world_b`.
    pub b_history_len: usize,
}

// ---------------------------------------------------------------------------
// compare — branch-vs-original or branch-vs-branch comparison
// ---------------------------------------------------------------------------

/// Compare two branch Worlds, reporting whether they have diverged and where.
///
/// Each world is typically the result of replaying (or live-running) a log to
/// completion. The function backs both a branch-vs-original comparison (pass
/// the folded original as `world_a` and the folded branch as `world_b`) and a
/// branch-vs-branch comparison (pass any two folded branch Worlds).
///
/// - When `world_a == world_b` (structurally identical via `PartialEq`), the
///   returned [`BranchDiff`] has `diverged = false` and `diverged_at = None`.
///   This is the no-op-edit case (VC-1.3): the branch World is byte-identical
///   to the original.
///
/// - When the worlds differ, `diverged = true` and `diverged_at =
///   Some(min(world_a.clock, world_b.clock))`. Because the unchanged prefix
///   `[0..fork_tick)` is replayed identically (VC-1.1), the divergence is
///   bounded by the minimum observed clock.
///
/// No business logic is embedded here; this is the pure seam the CLI
/// (`CR-cli-eval`) wraps to implement `replay diff`.
///
/// See docs/agent/world/ecs-runtime.md — Branch identity and storage layout.
pub fn compare(world_a: &World, world_b: &World) -> BranchDiff {
    let diverged = world_a != world_b;
    let diverged_at = if diverged {
        Some(world_a.clock.min(world_b.clock))
    } else {
        None
    };
    let a_history_len = world_a
        .entities
        .values()
        .map(|c| c.history.messages().len())
        .sum();
    let b_history_len = world_b
        .entities
        .values()
        .map(|c| c.history.messages().len())
        .sum();
    BranchDiff {
        diverged,
        diverged_at,
        a_clock: world_a.clock,
        b_clock: world_b.clock,
        a_history_len,
        b_history_len,
    }
}

// ---------------------------------------------------------------------------
// BranchStore — filesystem-backed listing and log access
// ---------------------------------------------------------------------------

/// A filesystem-backed store for reading branch metadata and event logs from a
/// session directory.
///
/// `BranchStore` is the READ counterpart to the WRITE pair [`fork`] +
/// [`persist_branch`]. It enumerates branches from `branches/index.jsonl` and
/// opens per-branch event logs under `branches/<branch_id>/world-events.jsonl`.
///
/// This is the seam the CLI wraps (`CR-cli-eval`): no business logic lives here
/// beyond listing and opening. Callers replay branches via
/// `replay::replay_world` or `replay::replay_branch` over the events returned
/// by [`BranchStore::load_events`].
///
/// See docs/agent/world/ecs-runtime.md — Branch identity and storage layout.
pub struct BranchStore {
    /// Root of the session: `branches/index.jsonl` and per-branch directories
    /// live directly beneath this path.
    session_dir: PathBuf,
}

impl BranchStore {
    /// Create a `BranchStore` rooted at `session_dir`.
    ///
    /// Lazy: no filesystem access occurs until [`list`][BranchStore::list] or
    /// [`load_events`][BranchStore::load_events] is called.
    pub fn new(session_dir: impl AsRef<Path>) -> Self {
        BranchStore {
            session_dir: session_dir.as_ref().to_owned(),
        }
    }

    /// List all branch descriptors recorded in `branches/index.jsonl`.
    ///
    /// Returns an empty `Vec` if the index file does not exist (no branches
    /// have been persisted yet). Each non-empty line is a JSON-serialized
    /// [`BranchDescriptor`]; a malformed line is returned as an error.
    ///
    /// Ordering is creation (append) order, matching the [`persist_branch`]
    /// append discipline.
    ///
    /// See docs/agent/world/ecs-runtime.md — Branch identity and storage layout.
    pub fn list(&self) -> Result<Vec<BranchDescriptor>, Error> {
        let index_path = self.session_dir.join("branches").join("index.jsonl");
        if !index_path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&index_path)?;
        let mut descriptors = Vec::new();
        for (line_num, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let descriptor: BranchDescriptor =
                serde_json::from_str(trimmed).map_err(|e| {
                    Error::World(format!(
                        "branches/index.jsonl line {}: {e}",
                        line_num + 1
                    ))
                })?;
            descriptors.push(descriptor);
        }
        Ok(descriptors)
    }

    /// Open a [`FilesystemEventLog`] for the branch identified by `branch_id`.
    ///
    /// The log path is `branches/<branch_id>/world-events.jsonl`, which under the
    /// tail-only layout holds ONLY the branch's divergent tail (events
    /// `>= fork_tick`). This is the APPEND target the live branch driver writes
    /// fresh results to; callers wanting the FULL ordered branch log (prefix
    /// resolved from the parent ++ tail) MUST use [`BranchStore::load_events`].
    /// The returned log is lazy (no I/O until `load` or `append` is called) so
    /// this call is always cheap.
    ///
    /// See docs/agent/world/ecs-runtime.md — Tail-only on-disk storage.
    pub fn open(&self, branch_id: &BranchId) -> FilesystemEventLog {
        let log_path = self
            .session_dir
            .join("branches")
            .join(&branch_id.0)
            .join("world-events.jsonl");
        FilesystemEventLog::new(log_path)
    }

    /// Load the FULL ordered stratum-1 event log for `branch_id`, reconstructing
    /// the shared prefix from the parent chain.
    ///
    /// Under tail-only storage a non-root branch persists only its tail (events
    /// `>= fork_tick`); the prefix `[0..fork_tick)` is shared by reference to the
    /// parent recorded in the [`BranchDescriptor`]. This method resolves that
    /// reference: it loads the parent's FULL log (recursively, so a branch of a
    /// branch resolves through its entire ancestor chain up to
    /// [`BranchId::MAIN`]), takes the prefix `[0..fork_tick)`, validates it
    /// against the descriptor's `prefix_digest` — a missing/mismatched parent
    /// fails loud rather than silently corrupting — then appends the branch's own
    /// tail. The result is BYTE-IDENTICAL to the prior full-clone load: prefix
    /// events (`at < fork_tick`) precede tail events (`at >= fork_tick`) in tick
    /// order, exactly as the materialized full clone stored them.
    ///
    /// [`BranchId::MAIN`] is the root recorded log stored in full at
    /// `<session_dir>/world-events.jsonl`; it has no prefix to resolve and is
    /// returned directly. A non-root branch with no descriptor, a cyclic parent
    /// chain, or a prefix-digest mismatch is an error (the branch log cannot be
    /// faithfully reconstructed).
    ///
    /// See docs/agent/world/ecs-runtime.md — Tail-only on-disk storage.
    pub fn load_events(&self, branch_id: &BranchId) -> Result<Vec<Event>, Error> {
        let index = self.descriptor_index()?;
        let mut visited = BTreeSet::new();
        self.load_full(branch_id, &index, &mut visited)
    }

    /// Build a `BranchId -> BranchDescriptor` map (latest-line-wins, matching the
    /// append-only `branches/index.jsonl` discipline) so prefix reconstruction
    /// can resolve a branch's parent + fork tick without re-scanning the index
    /// per ancestor. A `BTreeMap` (never a `HashMap`) keeps the lookup free of a
    /// hidden ordering input.
    fn descriptor_index(&self) -> Result<BTreeMap<BranchId, BranchDescriptor>, Error> {
        let mut map = BTreeMap::new();
        for descriptor in self.list()? {
            map.insert(descriptor.id.clone(), descriptor);
        }
        Ok(map)
    }

    /// Reconstruct the full ordered log for `branch_id` by resolving its prefix
    /// from the parent chain and appending its own tail.
    ///
    /// `visited` bounds the recursion: a malformed parent chain that cycles is
    /// rejected rather than looping forever.
    fn load_full(
        &self,
        branch_id: &BranchId,
        index: &BTreeMap<BranchId, BranchDescriptor>,
        visited: &mut BTreeSet<BranchId>,
    ) -> Result<Vec<Event>, Error> {
        // The root recorded log is stored in full at the session root and has no
        // prefix to resolve — read it directly.
        if *branch_id == BranchId::MAIN {
            return FilesystemEventLog::new(self.session_dir.join("world-events.jsonl")).load();
        }

        // Cycle guard: re-entering a branch already on the resolution path means
        // the parent chain loops — a corrupt index — so fail loud.
        if !visited.insert(branch_id.clone()) {
            return Err(Error::World(format!(
                "branch {} parent chain cycles; cannot reconstruct its prefix",
                branch_id.0
            )));
        }

        let descriptor = index.get(branch_id).ok_or_else(|| {
            Error::World(format!(
                "branch {} has no descriptor in branches/index.jsonl; cannot \
                 resolve its shared prefix",
                branch_id.0
            ))
        })?;

        // Resolve the prefix `[0..fork_tick)` by reference to the parent's FULL
        // log. The parent's prefix below `fork_tick` is immutable (append-only),
        // so this is stable even if the parent log later grows past `fork_tick`.
        let parent_full = self.load_full(&descriptor.parent, index, visited)?;
        let prefix: Vec<Event> = parent_full
            .into_iter()
            .filter(|e| e.at < descriptor.fork_tick)
            .collect();

        // Validate the resolved prefix against the recorded digest so a
        // mismatched/missing parent fails loud rather than silently corrupting.
        let resolved = digest_events(&prefix)?;
        if resolved != descriptor.prefix_digest {
            return Err(Error::World(format!(
                "branch {} prefix digest mismatch: the parent prefix [0..{}) does \
                 not match the recorded prefix_digest — refusing to reconstruct a \
                 corrupt branch log",
                branch_id.0, descriptor.fork_tick
            )));
        }

        // Append the branch's own tail (events `>= fork_tick` on disk). A missing
        // tail file loads as empty, leaving the resolved prefix intact.
        let tail = self.open(branch_id).load()?;
        let mut full = prefix;
        full.extend(tail);
        Ok(full)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::world::inputs::{
        Capabilities, Fingerprint, ModelMeta, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::world::{EdgeId, Effort, ModelConfig, Resources};

    fn user_message_input(text: &str) -> LogicalInput {
        LogicalInput::UserMessage {
            to: 0,
            text: text.into(),
        }
    }

    fn model_responded_input() -> LogicalInput {
        LogicalInput::ModelResponded {
            cmd: 0,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            blocks: Vec::new(),
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-test".into(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        }
    }

    fn sample_edit(text: &str) -> Edit {
        Edit {
            at_tick: 1,
            input: user_message_input(text),
            kind: EditKind::Replace,
        }
    }

    // -----------------------------------------------------------------------
    // BranchId::MAIN sentinel
    // -----------------------------------------------------------------------

    #[test]
    fn main_is_the_empty_string_sentinel() {
        assert_eq!(BranchId::MAIN, BranchId(String::new()));
        // Serializes as the empty string JSON value.
        let json = serde_json::to_string(&BranchId::MAIN).expect("serialize MAIN");
        assert_eq!(json, "\"\"");
    }

    // -----------------------------------------------------------------------
    // VC-1.4: id determinism — same inputs → same id
    // -----------------------------------------------------------------------

    #[test]
    fn branch_id_is_deterministic_for_the_same_inputs() {
        let edit = sample_edit("hello");
        let a = BranchId::compute(&BranchId::MAIN, 1, &edit).expect("compute id a");
        let b = BranchId::compute(&BranchId::MAIN, 1, &edit).expect("compute id b");
        assert_eq!(a, b, "same parent+tick+edit must produce the same BranchId");
        // A computed id is a 64-char hex digest.
        assert_eq!(a.0.len(), 64, "BranchId is a 64-char hex digest");
    }

    // -----------------------------------------------------------------------
    // id distinctness — distinct edits → distinct ids
    // -----------------------------------------------------------------------

    #[test]
    fn branch_id_is_distinct_for_different_edits() {
        let edit_a = sample_edit("hello");
        let edit_b = sample_edit("world");
        let a = BranchId::compute(&BranchId::MAIN, 1, &edit_a).expect("compute id a");
        let b = BranchId::compute(&BranchId::MAIN, 1, &edit_b).expect("compute id b");
        assert_ne!(a, b, "distinct edits must produce distinct BranchIds");
    }

    #[test]
    fn branch_id_is_distinct_for_different_ticks() {
        let edit = sample_edit("hello");
        let a = BranchId::compute(&BranchId::MAIN, 1, &edit).expect("tick 1");
        let b = BranchId::compute(&BranchId::MAIN, 2, &edit).expect("tick 2");
        assert_ne!(a, b, "distinct fork ticks must produce distinct BranchIds");
    }

    #[test]
    fn branch_id_is_distinct_for_different_parents() {
        let edit = sample_edit("hello");
        let parent_a = BranchId::compute(&BranchId::MAIN, 0, &sample_edit("first"))
            .expect("parent a");
        let parent_b = BranchId::compute(&BranchId::MAIN, 0, &sample_edit("second"))
            .expect("parent b");
        let a = BranchId::compute(&parent_a, 1, &edit).expect("child of parent_a");
        let b = BranchId::compute(&parent_b, 1, &edit).expect("child of parent_b");
        assert_ne!(a, b, "distinct parents must produce distinct BranchIds");
    }

    // -----------------------------------------------------------------------
    // VC-1.4: exogenous accept / derived reject
    // -----------------------------------------------------------------------

    #[test]
    fn edit_is_exogenous_accepts_user_message() {
        let input = user_message_input("hello");
        assert!(
            edit_is_exogenous(&input),
            "UserMessage is exogenous and must be accepted"
        );
    }

    #[test]
    fn edit_is_exogenous_accepts_session_started() {
        let input = LogicalInput::SessionStarted {
            seed: 42,
            surface_tools: Vec::new(),
        };
        assert!(
            edit_is_exogenous(&input),
            "SessionStarted is exogenous and must be accepted"
        );
    }

    #[test]
    fn edit_is_exogenous_rejects_model_responded() {
        let input = model_responded_input();
        assert!(
            !edit_is_exogenous(&input),
            "ModelResponded is a derived result and must be rejected"
        );
    }

    #[test]
    fn edit_is_exogenous_rejects_tool_returned() {
        let input = LogicalInput::ToolReturned {
            cmd: 0,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            result: Vec::new(),
        };
        assert!(
            !edit_is_exogenous(&input),
            "ToolReturned is a derived result and must be rejected"
        );
    }

    #[test]
    fn edit_is_exogenous_rejects_compacted() {
        let input = LogicalInput::Compacted {
            entity: 0,
            cmd: 0,
            fingerprint: Fingerprint(String::new()),
            summary: Vec::new(),
            replaced: 0,
        };
        assert!(
            !edit_is_exogenous(&input),
            "Compacted is a derived result and must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // BranchDescriptor round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn branch_descriptor_round_trips_via_serde() {
        let id = BranchId::compute(&BranchId::MAIN, 3, &sample_edit("hi"))
            .expect("compute id");
        let descriptor = BranchDescriptor {
            id: id.clone(),
            parent: BranchId::MAIN.clone(),
            fork_tick: 3,
            edit_summary: "changed greeting to hi".into(),
            created_wall: 1_700_000_000,
            prefix_digest: Fingerprint("abc123".into()),
            pin: false,
            tombstone: false,
        };

        let json = serde_json::to_string(&descriptor).expect("serialize BranchDescriptor");
        let restored: BranchDescriptor =
            serde_json::from_str(&json).expect("deserialize BranchDescriptor");

        assert_eq!(restored, descriptor, "BranchDescriptor must round-trip via serde");
        assert_eq!(restored.id, id);
        assert_eq!(restored.parent, BranchId::MAIN);
        assert_eq!(restored.fork_tick, 3);
        assert!(!restored.pin);
        assert!(!restored.tombstone);
    }

    #[test]
    fn branch_descriptor_round_trips_with_pin_and_tombstone() {
        let id = BranchId::compute(&BranchId::MAIN, 1, &sample_edit("x"))
            .expect("compute id");
        let descriptor = BranchDescriptor {
            id: id.clone(),
            parent: BranchId::MAIN.clone(),
            fork_tick: 1,
            edit_summary: "x".into(),
            created_wall: 0,
            prefix_digest: Fingerprint(String::new()),
            pin: true,
            tombstone: true,
        };

        let json = serde_json::to_string(&descriptor).expect("serialize");
        let restored: BranchDescriptor = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.pin, true);
        assert_eq!(restored.tombstone, true);
    }

    // -----------------------------------------------------------------------
    // fork — branch materialization fixtures
    // -----------------------------------------------------------------------

    fn ev(at: Tick, origin: Origin, edge: EdgeId, input: LogicalInput) -> Event {
        Event {
            origin,
            edge,
            at,
            wall: None,
            input,
        }
    }

    fn session_started() -> LogicalInput {
        LogicalInput::SessionStarted {
            seed: 0xABCD,
            surface_tools: Vec::new(),
        }
    }

    /// A small recorded log: the genesis header, a user turn, its derived model
    /// response (a carried result), and a follow-up user turn.
    fn sample_log() -> Vec<Event> {
        vec![
            ev(0, Origin::System, 0, session_started()),
            ev(1, Origin::Human, 0, user_message_input("hello")),
            ev(2, Origin::Agent, 0, model_responded_input()),
            ev(3, Origin::Human, 0, user_message_input("again")),
        ]
    }

    // -----------------------------------------------------------------------
    // VC-1.1 / VC-1.4: exogenous accepted, derived rejected
    // -----------------------------------------------------------------------

    #[test]
    fn fork_accepts_an_exogenous_edit() {
        let log = sample_log();
        let edit = Edit {
            at_tick: 1,
            input: user_message_input("hi"),
            kind: EditKind::Replace,
        };
        assert!(
            fork(&log, 1, edit).is_ok(),
            "an exogenous edit must be accepted"
        );
    }

    #[test]
    fn fork_rejects_a_derived_edit() {
        let log = sample_log();
        let edit = Edit {
            at_tick: 2,
            input: model_responded_input(),
            kind: EditKind::Replace,
        };
        assert!(
            fork(&log, 2, edit).is_err(),
            "editing a derived model result must be rejected (VC-1.4)"
        );
    }

    /// VC-1.4 (slot check): even when the EDIT INPUT itself is exogenous, a
    /// Replace at a tick whose ORIGINAL slot is a derived result (e.g.
    /// `ModelResponded`) must be rejected. The invariant is "DERIVED slots are
    /// NEVER edited in place"; only the edit-input check was insufficient.
    #[test]
    fn fork_replace_rejects_replacing_a_derived_slot() {
        let log = sample_log(); // tick 2 is ModelResponded (derived)
        let edit = Edit {
            at_tick: 2,
            input: user_message_input("try to replace model output"), // exogenous input…
            kind: EditKind::Replace, // …but the SLOT at tick 2 is derived
        };
        assert!(
            fork(&log, 2, edit).is_err(),
            "fork Replace at a derived slot must be rejected even when the edit \
             input is exogenous (VC-1.4 slot-check)"
        );
    }

    // -----------------------------------------------------------------------
    // VC-1.1: prefix verbatim, ticks monotonic, original untouched
    // -----------------------------------------------------------------------

    #[test]
    fn fork_replace_keeps_prefix_verbatim_and_ticks_monotonic() {
        let log = sample_log();
        let before = log.clone();
        let edit = Edit {
            at_tick: 1,
            input: user_message_input("hi"),
            kind: EditKind::Replace,
        };
        let (_, branch) = fork(&log, 1, edit).expect("fork");

        // Prefix [0..1) is byte-identical to the original.
        assert_eq!(branch[0], log[0], "prefix event must be verbatim");

        // The fork point carries the edited input at the original tick.
        assert_eq!(branch[1].input, user_message_input("hi"));
        assert_eq!(branch[1].at, 1);

        // The original derived result and the follow-up are carried (re-stamped),
        // so the branch can re-fingerprint and reuse them on replay.
        assert_eq!(branch[2].input, model_responded_input());
        assert_eq!(branch[3].input, user_message_input("again"));

        // Ticks are strictly increasing across the whole branch.
        for w in branch.windows(2) {
            assert!(
                w[0].at < w[1].at,
                "branch ticks must be strictly increasing: {} !< {}",
                w[0].at,
                w[1].at
            );
        }

        // The original log slice is untouched.
        assert_eq!(log, before, "fork must not mutate the original log");
    }

    #[test]
    fn fork_extend_inserts_after_the_anchor() {
        let log = sample_log();
        let edit = Edit {
            at_tick: 1,
            input: user_message_input("extra"),
            kind: EditKind::Extend,
        };
        let (_, branch) = fork(&log, 1, edit).expect("fork");

        // Extend adds exactly one event and keeps the anchor verbatim.
        assert_eq!(branch.len(), log.len() + 1, "extend adds exactly one event");
        assert_eq!(
            branch[1].input,
            user_message_input("hello"),
            "the anchor input is kept verbatim"
        );
        assert_eq!(
            branch[2].input,
            user_message_input("extra"),
            "the edit is inserted immediately after the anchor"
        );
        assert_eq!(branch[2].at, 2, "the inserted edit is re-stamped at fork+1");

        for w in branch.windows(2) {
            assert!(w[0].at < w[1].at, "branch ticks must be strictly increasing");
        }
    }

    // -----------------------------------------------------------------------
    // VC-1.1: deterministic BranchId
    // -----------------------------------------------------------------------

    #[test]
    fn fork_branch_id_is_deterministic_and_matches_compute() {
        let log = sample_log();
        let edit = Edit {
            at_tick: 1,
            input: user_message_input("hi"),
            kind: EditKind::Replace,
        };
        let (id_a, _) = fork(&log, 1, edit.clone()).expect("fork a");
        let (id_b, _) = fork(&log, 1, edit.clone()).expect("fork b");
        assert_eq!(id_a, id_b, "same inputs must yield the same BranchId");

        let expected = BranchId::compute(&BranchId::MAIN, 1, &edit).expect("compute");
        assert_eq!(
            id_a, expected,
            "fork id must equal BranchId::compute(MAIN, fork_tick, edit)"
        );
    }

    // -----------------------------------------------------------------------
    // Task-local: persist + reload round-trip (tempdir)
    // -----------------------------------------------------------------------

    /// Write `events` as the session's MAIN recorded log at
    /// `<session_dir>/world-events.jsonl`, the shared prefix source a first-level
    /// branch resolves against on load.
    fn write_main_log(session_dir: &Path, events: &[Event]) {
        let mut main = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
        for ev in events {
            main.append(ev).expect("append to MAIN log");
        }
    }

    /// VC-6.1: `persist_branch` writes the branch TAIL-ONLY (events `>= fork_tick`),
    /// and `BranchStore::load_events` reconstructs the full branch byte-identically
    /// by resolving the prefix from the parent (MAIN). The on-disk branch file
    /// therefore contains ONLY the tail; the prefix is shared by reference.
    #[test]
    fn persist_branch_writes_tail_only_and_load_reconstructs_full() {
        let log = sample_log();
        let edit = Edit {
            at_tick: 1,
            input: user_message_input("hi"),
            kind: EditKind::Replace,
        };
        let (id, branch) = fork(&log, 1, edit.clone()).expect("fork");

        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();
        // The parent MAIN log holds the shared prefix the branch resolves against.
        write_main_log(session_dir, &log);
        let descriptor = persist_branch(
            session_dir,
            &BranchId::MAIN,
            1,
            &edit,
            &branch,
            1_700_000_000,
        )
        .expect("persist branch");

        // The descriptor identity matches the fork id, and its prefix digest is
        // the digest of the verbatim prefix `[0..fork_tick)`.
        assert_eq!(descriptor.id, id, "persisted id must equal the fork id");
        let prefix: Vec<Event> = branch.iter().filter(|e| e.at < 1).cloned().collect();
        assert_eq!(
            descriptor.prefix_digest,
            digest_events(&prefix).expect("digest prefix"),
            "prefix_digest must be the digest of the verbatim prefix"
        );

        // The on-disk branch file holds ONLY the tail (events `>= fork_tick`); the
        // prefix is NOT cloned to disk.
        let events_path = session_dir
            .join("branches")
            .join(&id.0)
            .join("world-events.jsonl");
        let on_disk = FilesystemEventLog::new(&events_path)
            .load()
            .expect("reload branch tail");
        let expected_tail: Vec<Event> = branch.iter().filter(|e| e.at >= 1).cloned().collect();
        assert_eq!(
            on_disk, expected_tail,
            "the on-disk branch file must hold ONLY events `>= fork_tick` (the tail)"
        );
        assert!(
            on_disk.iter().all(|e| e.at >= 1),
            "no prefix event (`at < fork_tick`) may be cloned to the branch file"
        );

        // `load_events` reconstructs the FULL branch — byte-identical to the
        // materialized full-clone log (prefix resolved from the parent ++ tail).
        let store = BranchStore::new(session_dir);
        let reconstructed = store.load_events(&id).expect("load_events");
        assert_eq!(
            reconstructed, branch,
            "load_events must reconstruct the full branch byte-identically to the \
             prior full-clone load"
        );

        // The descriptor round-trips from the index.
        let index_path = session_dir.join("branches").join("index.jsonl");
        let index_text = std::fs::read_to_string(&index_path).expect("read index");
        let descriptors: Vec<BranchDescriptor> = index_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("parse descriptor"))
            .collect();
        assert_eq!(
            descriptors,
            vec![descriptor],
            "branches/index.jsonl must hold the persisted descriptor"
        );
    }

    // -----------------------------------------------------------------------
    // BranchStore helpers
    // -----------------------------------------------------------------------

    fn sample_model() -> ModelConfig {
        ModelConfig {
            model: "test-model".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    fn sample_world() -> World {
        World::new(0, Resources::new(0, sample_model()))
    }

    // -----------------------------------------------------------------------
    // Task-local: BranchStore::list returns persisted descriptors (tempdir)
    // -----------------------------------------------------------------------

    #[test]
    fn branch_store_list_returns_empty_when_no_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BranchStore::new(dir.path());
        let descriptors = store.list().expect("list");
        assert!(
            descriptors.is_empty(),
            "list must return an empty Vec when branches/index.jsonl does not exist"
        );
    }

    #[test]
    fn branch_store_list_returns_persisted_descriptors_in_creation_order() {
        let log = sample_log();
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();

        // Persist two distinct forks via persist_branch.
        let edit_a = Edit {
            at_tick: 1,
            input: user_message_input("fork-a"),
            kind: EditKind::Replace,
        };
        let (_, branch_a) = fork(&log, 1, edit_a.clone()).expect("fork a");
        let desc_a = persist_branch(
            session_dir,
            &BranchId::MAIN,
            1,
            &edit_a,
            &branch_a,
            1_000,
        )
        .expect("persist branch a");

        let edit_b = Edit {
            at_tick: 3,
            input: user_message_input("fork-b"),
            kind: EditKind::Replace,
        };
        let (_, branch_b) = fork(&log, 3, edit_b.clone()).expect("fork b");
        let desc_b = persist_branch(
            session_dir,
            &BranchId::MAIN,
            3,
            &edit_b,
            &branch_b,
            2_000,
        )
        .expect("persist branch b");

        // BranchStore::list must return both descriptors in creation order.
        let store = BranchStore::new(session_dir);
        let listed = store.list().expect("list");
        assert_eq!(listed.len(), 2, "list must return both persisted branches");
        assert_eq!(listed[0], desc_a, "first entry must be desc_a (creation order)");
        assert_eq!(listed[1], desc_b, "second entry must be desc_b");

        // The descriptors carry the correct fork_tick and parent.
        assert_eq!(listed[0].fork_tick, 1);
        assert_eq!(listed[0].parent, BranchId::MAIN);
        assert_eq!(listed[1].fork_tick, 3);
        assert_eq!(listed[1].parent, BranchId::MAIN);
    }

    #[test]
    fn branch_store_load_events_round_trips_the_branch_log() {
        let log = sample_log();
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();
        // The shared prefix lives in the parent MAIN log; tail-only `load_events`
        // resolves the prefix from it.
        write_main_log(session_dir, &log);

        let edit = Edit {
            at_tick: 1,
            input: user_message_input("store-test"),
            kind: EditKind::Replace,
        };
        let (id, branch) = fork(&log, 1, edit.clone()).expect("fork");
        persist_branch(session_dir, &BranchId::MAIN, 1, &edit, &branch, 0)
            .expect("persist");

        let store = BranchStore::new(session_dir);
        let reloaded = store.load_events(&id).expect("load_events");
        assert_eq!(
            reloaded, branch,
            "load_events must reconstruct the full branch the fork materialized"
        );
    }

    /// VC-6.1 (branch-of-branch): a nested fork's `load_events` resolves its
    /// prefix through the WHOLE parent chain (MAIN <- A <- B) and reconstructs B's
    /// full log byte-identically to B's materialized full clone. The on-disk file
    /// for each level holds only that level's tail.
    #[test]
    fn branch_store_load_events_reconstructs_branch_of_branch() {
        let log = sample_log();
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();
        write_main_log(session_dir, &log);

        // A: fork MAIN at tick 1 (replace the user turn). `branch_a` is A's full
        // materialized log; persist_branch stores only its tail.
        let edit_a = Edit {
            at_tick: 1,
            input: user_message_input("A edit"),
            kind: EditKind::Replace,
        };
        let (id_a, branch_a) = fork(&log, 1, edit_a.clone()).expect("fork A");
        persist_branch(session_dir, &BranchId::MAIN, 1, &edit_a, &branch_a, 0)
            .expect("persist A");

        // B: fork A's full log at tick 3 (the carried "again" user turn is
        // exogenous), parented on A — its prefix [0..3) is shared up through A.
        let edit_b = Edit {
            at_tick: 3,
            input: user_message_input("B edit"),
            kind: EditKind::Replace,
        };
        let (_id_off_main, branch_b) = fork(&branch_a, 3, edit_b.clone()).expect("fork B");
        let desc_b = persist_branch(session_dir, &id_a, 3, &edit_b, &branch_b, 0)
            .expect("persist B");

        // B's reconstructed log resolves the prefix through A (and MAIN) and is
        // byte-identical to B's materialized full clone.
        let store = BranchStore::new(session_dir);
        let reconstructed = store.load_events(&desc_b.id).expect("load_events B");
        assert_eq!(
            reconstructed, branch_b,
            "a branch-of-branch must reconstruct its full log through the parent chain"
        );

        // Each on-disk file holds only its own tail.
        let tail_b = store.open(&desc_b.id).load().expect("load B tail");
        assert!(
            tail_b.iter().all(|e| e.at >= 3),
            "B's on-disk file must hold only events `>= fork_tick` (3)"
        );
    }

    /// On-load validation: when the resolved parent prefix does not match the
    /// recorded `prefix_digest` (a corrupt/wrong parent log), `load_events` fails
    /// loud rather than silently reconstructing a corrupt branch.
    #[test]
    fn branch_store_load_events_rejects_a_prefix_digest_mismatch() {
        let log = sample_log();
        let dir = tempfile::tempdir().expect("tempdir");
        let session_dir = dir.path();
        write_main_log(session_dir, &log);

        let edit = Edit {
            at_tick: 1,
            input: user_message_input("mismatch-test"),
            kind: EditKind::Replace,
        };
        let (id, branch) = fork(&log, 1, edit.clone()).expect("fork");
        persist_branch(session_dir, &BranchId::MAIN, 1, &edit, &branch, 0)
            .expect("persist");

        // Corrupt the MAIN prefix: rewrite the parent log with a DIFFERENT genesis
        // header so the resolved prefix `[0..1)` no longer digests to the recorded
        // `prefix_digest`.
        let mut corrupted = log.clone();
        corrupted[0] = ev(
            0,
            Origin::System,
            0,
            LogicalInput::SessionStarted {
                seed: 0x9999,
                surface_tools: Vec::new(),
            },
        );
        std::fs::remove_file(session_dir.join("world-events.jsonl")).expect("remove MAIN log");
        write_main_log(session_dir, &corrupted);

        let store = BranchStore::new(session_dir);
        let err = store
            .load_events(&id)
            .expect_err("a prefix digest mismatch must fail loud");
        assert!(
            matches!(err, Error::World(ref m) if m.contains("prefix digest mismatch")),
            "the error must name the prefix digest mismatch, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Task-local: compare — identical worlds show no-divergence; divergent
    // worlds report the right divergence point.
    // -----------------------------------------------------------------------

    #[test]
    fn compare_identical_worlds_are_not_diverged() {
        let world = sample_world();
        let diff = compare(&world, &world);
        assert!(!diff.diverged, "identical worlds must not be reported as diverged");
        assert_eq!(diff.diverged_at, None, "no divergence tick for identical worlds");
        assert_eq!(diff.a_clock, world.clock, "a_clock must equal world.clock");
        assert_eq!(diff.b_clock, world.clock, "b_clock must equal world.clock");
    }

    #[test]
    fn compare_cloned_identical_worlds_are_not_diverged() {
        let world = sample_world();
        let clone = world.clone();
        let diff = compare(&world, &clone);
        assert!(!diff.diverged, "a clone of a world must compare as identical");
        assert_eq!(diff.diverged_at, None);
    }

    #[test]
    fn compare_divergent_worlds_report_divergence_at_min_clock() {
        // Create world_a at tick 0 and world_b at tick 5 — they differ.
        let world_a = sample_world(); // clock = 0
        let mut world_b = sample_world();
        world_b.clock = 5; // advance the clock so the worlds structurally differ

        let diff = compare(&world_a, &world_b);
        assert!(diff.diverged, "worlds that differ must be reported as diverged");
        assert_eq!(
            diff.diverged_at,
            Some(world_a.clock.min(world_b.clock)),
            "diverged_at must be min(a_clock, b_clock)"
        );
        assert_eq!(diff.a_clock, 0);
        assert_eq!(diff.b_clock, 5);
    }

    #[test]
    fn compare_same_clock_different_resources_reports_divergence() {
        // Both worlds at clock 0 but with different RNG seeds — they differ.
        let world_a = World::new(0, Resources::new(42, sample_model()));
        let world_b = World::new(0, Resources::new(99, sample_model()));

        let diff = compare(&world_a, &world_b);
        assert!(diff.diverged, "worlds with different seeds must be diverged");
        assert_eq!(
            diff.diverged_at,
            Some(0),
            "diverged_at is the min of both clocks (both at 0)"
        );
    }

    #[test]
    fn compare_history_lengths_are_summed_over_all_entities() {
        // A world with no entities has a_history_len = 0.
        let world = sample_world();
        let diff = compare(&world, &world);
        assert_eq!(diff.a_history_len, 0, "empty entities → a_history_len = 0");
        assert_eq!(diff.b_history_len, 0, "empty entities → b_history_len = 0");
    }
}
