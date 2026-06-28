//! Thin CLI shell for `cargo xtask replay …`.
//!
//! Parses subcommand arguments, resolves session paths via
//! [`rubberdux::session::SessionManager`], and delegates the heavy lifting to
//! library functions.  No business logic lives here — the rule mirrors
//! `xtask/src/sessions.rs` and the rest of the xtask surface.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rubberdux::agent::world::branch::{
    compare, fork as branch_fork, persist_branch, BranchId, BranchStore, Edit, EditKind,
};
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{Command, SurfaceDriver};
use rubberdux::agent::world::event_log::{EventLog, FilesystemEventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::History;
use rubberdux::agent::world::inputs::{Event, Fingerprint, LogicalInput};
use rubberdux::agent::world::model_client::MessagesClient;
use rubberdux::agent::world::reclaim::{
    drop_branch as reclaim_drop_branch, load_latest_descriptors, pin as reclaim_pin,
    reclaim_branches, Roots,
};
use rubberdux::agent::world::replay::{
    fold_log, genesis_from_log, is_model_call_result, replay_branch, restore as restore_world,
};
use rubberdux::agent::world::segment::SegmentedEventLog;
use rubberdux::agent::world::snapshot::{capture, SnapshotStore};
use rubberdux::agent::world::world::{
    Activity, Caps, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, Timestamp,
    World,
};
use rubberdux::error::Error;
use rubberdux::session::SessionManager;

/// Print a session listing: each session with its hot/cold snapshot counts,
/// branch names, and pinned branches.  If `session_filter` is `Some(id)` only
/// that session is shown.
///
/// The on-disk layout probed here is the canonical artifact layout:
///   `<session>/snapshots/`       — hot `.snapshot.json` files
///   `<session>/snapshots/cold/`  — cold `.snapshot.json` files (after tiering)
///   `<session>/branches/`        — one sub-directory per branch
///   `<session>/pins/`            — one entry per pinned branch (reachability root)
///
/// None of `SnapshotStore`, `BranchStore`, or `SegmentedEventLog` need to exist
/// yet; this is a plain directory listing suitable for the CR-cli-core scaffolding
/// phase.  Pins are a reachability-root toggle in the design (`BranchDescriptor.pin`);
/// until that API lands this probes a sibling `pins/` directory, mirroring how
/// branches are listed.  CR-cli-eval and CR-cli-prune enrich the output once those
/// APIs land.
pub fn list(session_filter: Option<&str>) -> Result<(), String> {
    let manager = SessionManager::new();
    let sessions_dir = manager.sessions_dir();

    if !sessions_dir.exists() {
        println!("No sessions directory found at {}", sessions_dir.display());
        return Ok(());
    }

    // Resolve the session names to display.
    let session_names: Vec<String> = if let Some(id) = session_filter {
        let session_dir = sessions_dir.join(id);
        if !session_dir.exists() {
            return Err(format!("session not found: {}", id));
        }
        vec![id.to_string()]
    } else {
        let mut names: Vec<String> = fs::read_dir(sessions_dir)
            .map_err(|e| format!("failed to read sessions dir: {}", e))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    };

    if session_names.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    // Resolve the `latest` symlink for annotation.
    let latest_name: Option<String> = {
        let link = manager.latest_link();
        if link.exists() {
            fs::read_link(link)
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        } else {
            None
        }
    };

    for session_id in &session_names {
        let session_dir = sessions_dir.join(session_id);
        let latest_marker = if latest_name.as_deref() == Some(session_id.as_str()) {
            " (latest)"
        } else {
            ""
        };

        println!("session {}{}", session_id, latest_marker);

        // Snapshots — hot tier.
        let snapshots_dir = session_dir.join("snapshots");
        let hot = collect_snapshot_names(&snapshots_dir);
        // Snapshots — cold tier (moved here by the segment/retention shell).
        let cold_dir = snapshots_dir.join("cold");
        let cold = collect_snapshot_names(&cold_dir);

        println!(
            "  snapshots: {} hot, {} cold",
            hot.len(),
            cold.len()
        );
        for name in &hot {
            println!("    hot  {}", name);
        }
        for name in &cold {
            println!("    cold {}", name);
        }

        // Branches.
        let branches_dir = session_dir.join("branches");
        let branches = collect_branch_names(&branches_dir);
        println!("  branches: {}", branches.len());
        for name in &branches {
            println!("    branch {}", name);
        }

        // Pins — the reachability roots that protect a branch from dead-branch GC.
        let pins_dir = session_dir.join("pins");
        let pins = collect_pin_names(&pins_dir);
        println!("  pins: {}", pins.len());
        for name in &pins {
            println!("    pin {}", name);
        }
    }

    Ok(())
}

/// Return a sorted list of `.snapshot.json` filenames found directly under `dir`.
/// Returns an empty list if `dir` does not exist or cannot be read.
fn collect_snapshot_names(dir: &std::path::Path) -> Vec<String> {
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let names: BTreeSet<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.ends_with(".snapshot.json") && e.file_type().map(|t| t.is_file()).unwrap_or(false)
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.into_iter().collect()
}

/// Return a sorted list of branch directory names found directly under `dir`.
/// Returns an empty list if `dir` does not exist or cannot be read.
fn collect_branch_names(dir: &std::path::Path) -> Vec<String> {
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let names: BTreeSet<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.into_iter().collect()
}

/// Return a sorted list of pin entry names found directly under `dir`.
/// Returns an empty list if `dir` does not exist or cannot be read.
///
/// Every entry is accepted regardless of file type: the pin format is not yet
/// settled (a pin may land as a marker file named after its branch or as a
/// sub-directory), so this placeholder lists whatever names the directory holds.
fn collect_pin_names(dir: &std::path::Path) -> Vec<String> {
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let names: BTreeSet<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.into_iter().collect()
}

// ---------------------------------------------------------------------------
// restore — reconstruct a session to a tick and print its tick + World digest
// ---------------------------------------------------------------------------

/// Reconstruct the World of `session_id` at `at` (the latest recorded tick when
/// `at` is `None`) via [`rubberdux::agent::world::replay::restore`], and print
/// the resulting tick and content digest.
///
/// A THIN wrapper over the Phase-A API: resolve the session dir, load the
/// stratum-1 events and their stratum-2 lifecycle, call `restore` (nearest
/// snapshot ≤ target + tail fold + bounded resume), and `capture` the result for
/// its tick + digest. The bounded-resume reconciliations are folded into a
/// throwaway [`MemoryEventLog`] so inspecting a session never mutates its log.
///
/// See docs/agent/world/ecs-runtime.md — RESTORE; VC-5.1.
pub fn restore(session_id: &str, at: Option<u64>) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let log = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
    let events = log
        .load()
        .map_err(|e| format!("failed to load events: {e}"))?;
    if events.is_empty() {
        return Err(format!("session {session_id} has no recorded events"));
    }
    let lifecycle = log
        .load_lifecycle()
        .map_err(|e| format!("failed to load lifecycle: {e}"))?;

    let store = SnapshotStore::new(session_dir.join("snapshots"));
    let model = resolve_model_config();
    let target = at.unwrap_or_else(|| events.iter().map(|e| e.at).max().unwrap_or(0));

    // The resume phase appends `Settled` reconciliations to this log; a throwaway
    // memory log keeps `restore` non-destructive (the returned World is the
    // pre-resume reconstruction regardless of where `log` points).
    let mut sink = MemoryEventLog::new();
    let world = restore_world(
        &store,
        &events,
        &lifecycle,
        |seed| build_genesis_world(seed, model.clone()),
        target,
        &mut sink,
    )
    .map_err(|e| format!("restore failed: {e}"))?;

    let snapshot = capture(world);
    println!(
        "restored session {session_id} to tick {} (digest {})",
        snapshot.tick, snapshot.digest.0
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// fork — materialize a counterfactual branch from one exogenous edit
// ---------------------------------------------------------------------------

/// Fork `session_id` at `at`, applying the exogenous edit parsed from
/// `edit_spec`, persist the branch, and print its deterministic [`BranchId`].
///
/// A THIN wrapper over [`rubberdux::agent::world::branch::fork`] +
/// [`persist_branch`]: parse `edit_spec` into an [`Edit`] (`user:<text>` → a
/// `UserMessage`), then `fork` — which validates the edit targets an EXOGENOUS
/// input and REJECTS a derived model/tool result with a clear error (mapped to a
/// non-zero exit by the caller, VC-1.4). `persist_branch` writes the branch log +
/// descriptor under `branches/`.
///
/// See docs/agent/world/ecs-runtime.md — FORK/EDIT; VC-5.1.
pub fn fork(session_id: &str, at: u64, edit_spec: &str) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let log = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
    let events = log
        .load()
        .map_err(|e| format!("failed to load events: {e}"))?;
    if events.is_empty() {
        return Err(format!("session {session_id} has no recorded events"));
    }

    let edit = parse_edit_spec(edit_spec, at)?;
    // `fork` is the single edit-scope gate: it rejects a non-exogenous edit (and a
    // Replace targeting a derived slot) with `Error::World` — surfaced here as a
    // non-zero-exit error, never a panic.
    let (id, branch_events) =
        branch_fork(&events, at, edit.clone()).map_err(|e| format!("fork rejected: {e}"))?;

    let _descriptor = persist_branch(
        &session_dir,
        &BranchId::MAIN,
        at,
        &edit,
        &branch_events,
        unix_secs(),
    )
    .map_err(|e| format!("failed to persist branch: {e}"))?;

    println!("{}", id.0);
    Ok(())
}

// ---------------------------------------------------------------------------
// run — drive a branch replay → live to quiescence (LIVE, paid)
// ---------------------------------------------------------------------------

/// Drive the branch `branch` of `session_id` through
/// [`rubberdux::agent::world::replay::replay_branch`] to quiescence, going LIVE
/// past the edit's divergence and appending the fresh tail to the branch log.
///
/// A THIN wrapper over `replay_branch`: load the branch events, reseed its
/// genesis, then run the single replay→live mode with the PRODUCTION
/// [`MessagesClient`] (so this makes real, PAID model calls past the divergence)
/// and a no-op CLI surface sink. The resulting World's tick + digest are printed.
///
/// See docs/agent/world/ecs-runtime.md — Branch replay (replay→live handoff); VC-5.1.
pub async fn run(session_id: &str, branch: &str) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let store = BranchStore::new(&session_dir);
    let branch_id = BranchId(branch.to_string());
    let branch_events = store
        .load_events(&branch_id)
        .map_err(|e| format!("failed to load branch {branch}: {e}"))?;
    if branch_events.is_empty() {
        return Err(format!(
            "branch {branch} has no recorded events under session {session_id}"
        ));
    }

    let model = resolve_model_config();
    let genesis = genesis_from_log(&branch_events, |seed| build_genesis_world(seed, model.clone()))
        .map_err(|e| format!("failed to reseed branch genesis: {e}"))?;
    let client = MessagesClient::from_env().map_err(|e| format!("model client: {e}"))?;
    let surface = CliSurfaceDriver;

    // The live tail appends to the branch's own log (prefix-by-reference + edit +
    // fresh live results), exactly as `BranchStore::open` resolves it.
    let mut branch_log = store.open(&branch_id);
    let world = replay_branch(
        genesis,
        &branch_events,
        is_model_call_result,
        &client,
        &surface,
        &mut branch_log,
    )
    .await
    .map_err(|e| format!("branch replay failed: {e}"))?;

    let snapshot = capture(world);
    println!(
        "ran branch {branch} to quiescence: tick {} (digest {})",
        snapshot.tick, snapshot.digest.0
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// diff — compare two reconstructed branch (or branch-vs-original) Worlds
// ---------------------------------------------------------------------------

/// Reconstruct the Worlds of `branch_a` and `branch_b` under `session_id` and
/// print their divergence tick + summary via
/// [`rubberdux::agent::world::branch::compare`].
///
/// A THIN wrapper over `compare`: fold each operand's log to its final World and
/// compare. Either operand may be a branch id or `main`/`original` (the session's
/// own recorded log), so this backs both a branch-vs-original and a
/// branch-vs-branch diff.
///
/// See docs/agent/world/ecs-runtime.md — `compare` / BranchDiff; VC-5.1.
pub fn diff(session_id: &str, branch_a: &str, branch_b: &str) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let model = resolve_model_config();
    let world_a = reconstruct_world(&session_dir, branch_a, &model)?;
    let world_b = reconstruct_world(&session_dir, branch_b, &model)?;

    let diff = compare(&world_a, &world_b);
    match diff.diverged_at {
        Some(tick) => println!(
            "{branch_a} vs {branch_b}: diverged at tick {tick} \
             (a_clock={} b_clock={} a_history={} b_history={})",
            diff.a_clock, diff.b_clock, diff.a_history_len, diff.b_history_len
        ),
        None => println!(
            "{branch_a} vs {branch_b}: identical \
             (clock={} history={})",
            diff.a_clock, diff.a_history_len
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// prune — retention + cold-tier + dead-branch GC for one session
// ---------------------------------------------------------------------------

/// Prune `session` (the latest session when `None`): drive snapshot retention,
/// log-segment cold-tiering, and dead-branch reachability GC, then print the
/// evicted / tiered / reclaimed / protected counts.
///
/// A THIN wrapper over the Phase-C API — no policy lives here:
///   1. [`SnapshotStore::prune`] (retention) keeps the latest `keep` snapshots,
///      evicting the regenerable older ones.
///   2. [`SegmentedEventLog::tier_cold_storable`] MOVES every sealed segment
///      fully covered below the protection floor (the oldest retained snapshot's
///      tick) into `cold/` — never deleting it.
///   3. [`reclaim_branches`] reclaims dead, unreachable branches under [`Roots`]
///      seeded by the live session (`MAIN`, protecting the shared prefix) and the
///      branch-grace window; the descriptors carry the pin/drop state that
///      `drop`/`pin`/`unpin` toggled, so the toggle is consumed here.
///
/// `keep` defaults to the snapshot-retention cap ([`Caps::snapshot_keep`], 3).
/// `now` is a single wall-clock read passed to the pure reclaim core.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE; VC-5.1.
pub fn prune(session: Option<&str>, keep: Option<u32>) -> Result<(), String> {
    let (session_id, session_dir) = resolve_session_or_latest(session)?;
    // A single wall-clock read; the pure reclaim core is told `now` (no now() in
    // the core).
    let now = unix_secs();

    // The recorded log gives the live frontier (highest tick) for retention and
    // the live World's caps (the branch-grace reachability root).
    let log = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
    let events = log
        .load()
        .map_err(|e| format!("failed to load events: {e}"))?;
    if events.is_empty() {
        return Err(format!("session {session_id} has no recorded events"));
    }
    let live_tail = events.iter().map(|e| e.at).max().unwrap_or(0);

    // The keep window defaults to the snapshot-retention cap.
    let keep = keep.unwrap_or_else(|| Caps::default().snapshot_keep);

    // Reconstruct the live World to read its caps; the branch-grace window is the
    // time-dependent reachability root. Folding the recorded log is the lightest
    // faithful read (mirrors the `diff` handler's reconstruction).
    let model = resolve_model_config();
    let genesis = genesis_from_log(&events, |seed| build_genesis_world(seed, model.clone()))
        .map_err(|e| format!("failed to reseed genesis: {e}"))?;
    let world = fold_log(genesis, &events);
    let grace = world.resources.caps.branch_grace;

    // 1. Snapshot retention — keep the latest K, evicting older (regenerable).
    let store = SnapshotStore::new(session_dir.join("snapshots"));
    let evicted = store
        .prune(keep, live_tail)
        .map_err(|e| format!("snapshot retention failed: {e}"))?;

    // 2. Cold-tier fully-covered sealed segments below the protection floor (the
    //    oldest retained snapshot's tick); segments are MOVED, never deleted. A
    //    floor of 0 (no retained snapshot) tiers nothing.
    let floor = store
        .list_ticks()
        .map_err(|e| format!("failed to list retained snapshots: {e}"))?
        .into_iter()
        .min()
        .unwrap_or(0);
    let mut segmented = SegmentedEventLog::open(&session_dir)
        .map_err(|e| format!("failed to open segmented log: {e}"))?;
    let tiered = segmented
        .tier_cold_storable(floor)
        .map_err(|e| format!("cold-tiering failed: {e}"))?;

    // 3. Dead-branch GC — reclaim unreachable, dead branches. Reachable branches
    //    (live session / pinned / parent-of-retained) and the shared parent
    //    prefix are never touched. `protected` is the count of branch descriptors
    //    this pass retained.
    let roots = Roots::new(BranchId::MAIN, grace);
    let total_branches = load_latest_descriptors(&session_dir)
        .map_err(|e| format!("failed to load branch descriptors: {e}"))?
        .len();
    let reclaimed = reclaim_branches(&session_dir, &roots, now)
        .map_err(|e| format!("dead-branch GC failed: {e}"))?;
    let protected = total_branches.saturating_sub(reclaimed.len());

    println!(
        "pruned session {session_id}: {} evicted, {} tiered, {} reclaimed, {} protected",
        evicted.len(),
        tiered.len(),
        reclaimed.len(),
        protected
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// drop / pin / unpin — branch reachability-root toggles (consumed by prune)
// ---------------------------------------------------------------------------

/// Drop `branch` of `session_id` — tombstone it so the next [`prune`] reclaims
/// it. A THIN wrapper over [`reclaim_drop_branch`]; errors (non-zero exit) when
/// no such branch exists.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC;
/// VC-5.1.
pub fn drop_branch(session_id: &str, branch: &str) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let id = BranchId(branch.to_string());
    reclaim_drop_branch(&session_dir, &id)
        .map_err(|e| format!("failed to drop branch {branch}: {e}"))?;
    println!("dropped branch {branch} in session {session_id}");
    Ok(())
}

/// Pin `branch` of `session_id` as a reachability root so [`prune`] never
/// reclaims it. A THIN wrapper over [`reclaim_pin`]`(.., true)`; errors when no
/// such branch exists.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC;
/// VC-5.1.
pub fn pin(session_id: &str, branch: &str) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let id = BranchId(branch.to_string());
    reclaim_pin(&session_dir, &id, true)
        .map_err(|e| format!("failed to pin branch {branch}: {e}"))?;
    println!("pinned branch {branch} in session {session_id}");
    Ok(())
}

/// Unpin `branch` of `session_id` — clear its reachability-root flag. A THIN
/// wrapper over [`reclaim_pin`]`(.., false)`; errors when no such branch exists.
///
/// See docs/agent/world/ecs-runtime.md — PRUNE / dead-branch reachability GC;
/// VC-5.1.
pub fn unpin(session_id: &str, branch: &str) -> Result<(), String> {
    let session_dir = resolve_session_dir(session_id)?;
    let id = BranchId(branch.to_string());
    reclaim_pin(&session_dir, &id, false)
        .map_err(|e| format!("failed to unpin branch {branch}: {e}"))?;
    println!("unpinned branch {branch} in session {session_id}");
    Ok(())
}

// ---------------------------------------------------------------------------
// shared helpers — session/path/genesis resolution + edit parsing
// ---------------------------------------------------------------------------

/// Resolve `session_id` to its on-disk session directory under the
/// [`SessionManager`] layout, erroring (non-zero exit) when it does not exist.
fn resolve_session_dir(session_id: &str) -> Result<PathBuf, String> {
    let manager = SessionManager::new();
    let dir = manager.sessions_dir().join(session_id);
    if !dir.exists() {
        return Err(format!("session not found: {session_id}"));
    }
    Ok(dir)
}

/// Resolve the session to operate on for [`prune`]: the explicit `--session`
/// when given, else the `latest` link (mirroring the `list` handler's latest
/// resolution). Returns the session id and its on-disk directory, erroring when
/// neither a `--session` nor a latest session is available.
fn resolve_session_or_latest(session: Option<&str>) -> Result<(String, PathBuf), String> {
    if let Some(id) = session {
        return Ok((id.to_string(), resolve_session_dir(id)?));
    }
    let manager = SessionManager::new();
    let link = manager.latest_link();
    let name = fs::read_link(link)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .ok_or_else(|| "no --session given and no latest session found".to_string())?;
    let dir = resolve_session_dir(&name)?;
    Ok((name, dir))
}

/// Fold an operand's recorded log to its final World for [`diff`]. `name` is a
/// branch id, or `main`/`original` to fold the session's own recorded log.
fn reconstruct_world(
    session_dir: &Path,
    name: &str,
    model: &ModelConfig,
) -> Result<World, String> {
    let events = load_events_for(session_dir, name)?;
    if events.is_empty() {
        return Err(format!("{name} has no recorded events under the session"));
    }
    let genesis = genesis_from_log(&events, |seed| build_genesis_world(seed, model.clone()))
        .map_err(|e| format!("failed to reseed genesis for {name}: {e}"))?;
    Ok(fold_log(genesis, &events))
}

/// Load the recorded events for `name`: a branch id (via [`BranchStore`]), or the
/// session's own `world-events.jsonl` when `name` is `main`/`original`.
fn load_events_for(session_dir: &Path, name: &str) -> Result<Vec<Event>, String> {
    if name.eq_ignore_ascii_case("main") || name.eq_ignore_ascii_case("original") {
        FilesystemEventLog::new(session_dir.join("world-events.jsonl"))
            .load()
            .map_err(|e| format!("failed to load original log: {e}"))
    } else {
        BranchStore::new(session_dir)
            .load_events(&BranchId(name.to_string()))
            .map_err(|e| format!("failed to load branch {name}: {e}"))
    }
}

/// Reconstruct the genesis [`ModelConfig`] the live run used. The world-default
/// model is not recorded in the event log, so the caller supplies the same value
/// the live run resolved — read from the same `RUBBERDUX_LLM_*` environment the
/// production worker reads (mirrors `app::runtime::worker::world_model_config` and
/// [`MessagesClient::from_env`]'s `RUBBERDUX_LLM_MODEL` default).
fn resolve_model_config() -> ModelConfig {
    let model = std::env::var("RUBBERDUX_LLM_MODEL")
        .unwrap_or_else(|_| "claude-opus-4-5-20251101".into());
    let max_tokens = std::env::var("RUBBERDUX_LLM_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(4096);
    ModelConfig {
        model,
        max_tokens,
        effort: Effort::Medium,
    }
}

/// Build the genesis `World` a session starts from: tick 0, a single primary
/// `Idle` root entity, `Resources` reseeded from `seed` with `model` as the
/// world-default. Mirrors `WorldDriver::bootstrap`'s genesis so a reconstruction
/// reseeds identically to the live run (Inv 8).
fn build_genesis_world(seed: u64, model: ModelConfig) -> World {
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
            model: None,
        },
    );
    world
}

/// Parse an `<exo-spec>` into an [`Edit`] applied at `at_tick` as a `Replace`.
///
/// Grammar `"<kind>:<payload>"`:
///   - `user:<text>` → a `UserMessage` (EXOGENOUS — accepted by `fork`).
///   - `tool:<text>` → a `ToolReturned` (DERIVED — `fork` REJECTS it; this is how
///     a non-exogenous edit is surfaced and rejected, VC-1.4).
///
/// Any other kind, or a spec without a `:`, is a parse error (non-zero exit).
fn parse_edit_spec(spec: &str, at_tick: u64) -> Result<Edit, String> {
    let (kind, payload) = spec.split_once(':').ok_or_else(|| {
        format!("malformed --edit '{spec}': expected '<kind>:<text>' (e.g. user:hello)")
    })?;
    let input = match kind {
        "user" => LogicalInput::UserMessage {
            to: 0,
            text: payload.to_string(),
        },
        // A derived result, parsed only so `fork`'s exogenous gate can reject it.
        "tool" => LogicalInput::ToolReturned {
            cmd: 0,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            result: Vec::new(),
        },
        other => {
            return Err(format!(
                "unknown --edit kind '{other}': expected 'user' (exogenous) or 'tool' (derived)"
            ));
        }
    };
    Ok(Edit {
        at_tick,
        input,
        kind: EditKind::Replace,
    })
}

/// The current wall-clock as Unix seconds, the `created_wall` stamp passed to
/// [`persist_branch`] (the pure core has no `now()`; the shell supplies it).
fn unix_secs() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as Timestamp)
        .unwrap_or(0)
}

/// A no-op surface-drive sink for the CLI: `replay run` drives a branch to
/// quiescence without a live macOS app attached, so a `set_value` surface drive
/// is accepted and dropped (the recorded `ToolReturned` still folds). Mirrors the
/// worker's production `SurfaceDriver`, minus the host channel.
struct CliSurfaceDriver;

impl SurfaceDriver for CliSurfaceDriver {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}
