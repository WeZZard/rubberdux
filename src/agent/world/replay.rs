// These items are the replay spine consumed by the integration tests now and by the
// snapshot/restore/branch tasks (CR-snapshot, CR-restore, CR-branch-id, CR-branch-replay)
// next; the binary target has no caller yet, so silence dead-code lints here.
#![allow(dead_code)]

//! The replay spine: reconstruct a `World` deterministically from a recorded log.
//!
//! Three reconstruction primitives, promoted from the (formerly duplicated)
//! test-only harnesses so snapshot, restore, and branch-replay can all build on
//! one implementation:
//!
//! - [`genesis_from_log`] — reseed the fresh genesis `World` from the log's
//!   `SessionStarted` header (the seed is the one hidden input that must cross a
//!   recorded boundary, Inv 8).
//! - [`fold_log`] — fold the WHOLE recorded log through the pure `tick` reducer:
//!   every input (the exogenous free variables AND the already-recorded derived
//!   results) is present, so emitted Commands are discarded. This is the canonical
//!   ("live") `World` a replay must reproduce byte-for-byte.
//! - [`replay_world`] — fold the SAME log from genesis under the REPLAY driver:
//!   re-apply the recorded results' COMPLEMENT directly and, for each tick's
//!   Commands, call `effects::drive_replay` — which takes NO `ModelCaller`, so it
//!   cannot reach the model by construction — standing in the already-logged result
//!   while the re-emitted request re-hashes to its `Fingerprint` (Inv 7). A
//!   `Diverged` outcome is a replay failure: a faithful replay reuses every result.
//! - [`restore`] — reconstruct the `World` at a target tick from the NEAREST
//!   snapshot ≤ target (else genesis) plus a TAIL fold of the log segment the
//!   snapshot has not yet absorbed, then bounded-[`resume`] over the
//!   reconstructed World's [`outstanding_cmds`] so an in-flight effect that was
//!   dispatched before the crash settles deterministically (Inv 5/9/17). The
//!   reconstruction is provably identical to a full replay from genesis because
//!   folding is associative — a snapshot is itself a fold-prefix.
//! - [`replay_branch`] — the COUNTERFACTUAL driver: replay the branch's recorded
//!   prefix exactly as `replay_world` does, but at the FIRST `Diverged` flip
//!   ONE-WAY to the LIVE driver (`effects::drive_live`) and run the remainder of
//!   the branch live (fresh, paid results appended to the branch log). A no-op edit
//!   re-fingerprints identically and reuses the ENTIRE tail, so an unchanged branch
//!   is byte-identical to `replay_world` and the live client never fires. This is
//!   the single Mode{Replay,Live} that flips EXACTLY ONCE (Inv 6/7).
//!
//! The partition between "recorded result" (stood in by the `ReplayCursor`) and
//! "re-applied directly" is supplied by the caller as a predicate, because it is
//! NARROWER than `effects::is_derived_result` while the `RunTool` /
//! `RequestHumanAction` replay drivers are still stubs: `drive_replay` only stands
//! in for `CallModel` / `Compact`, so a tool-bearing log re-applies its
//! `ToolReturned` / `HumanActionDone` results directly. [`is_model_call_result`] is
//! that narrower predicate; [`is_exogenous`] is the exact complement of
//! `effects::is_derived_result`, exposed for callers that partition on the full
//! derived set.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), Inv 7
//! (content-addressed replay — the fingerprint), Inv 8 (recorded seed), Inv 9
//! (the log is the single source of truth).

use std::collections::BTreeSet;

use crate::agent::world::effects::{
    drive_live, drive_replay, resume, Command, ModelCaller, Reconciliation, ReplayCursor, Replayed,
    ResultStamp, SurfaceDriver,
};
use crate::agent::world::event_log::EventLog;
use crate::agent::world::inputs::{Event, LogicalInput};
use crate::agent::world::lifecycle::LifecycleEvent;
use crate::agent::world::snapshot::SnapshotStore;
use crate::agent::world::systems::tick;
use crate::agent::world::world::{Activity, CmdId, EdgeId, HELD_CMD, SlotState, Tick, World};
use crate::error::Error;

/// The RNG seed recorded in the log's `SessionStarted` header — the one hidden
/// input that must cross a recorded boundary so replay reseeds `Rng` identically
/// to the live run (Inv 8).
fn seed_from_log(events: &[Event]) -> Result<u64, Error> {
    events
        .iter()
        .find_map(|e| match &e.input {
            LogicalInput::SessionStarted { seed, .. } => Some(*seed),
            _ => None,
        })
        .ok_or_else(|| Error::World("recorded log has no SessionStarted header".into()))
}

/// Rebuild the genesis `World` from a recorded log: extract the recorded seed and
/// hand it to `build_genesis`, which constructs the fresh `World` exactly as the
/// live path did (the world-default `ModelConfig` is not recorded in P0, so the
/// caller supplies the SAME value the live run used — documented genesis-parity,
/// not a live re-resolution).
pub fn genesis_from_log(
    events: &[Event],
    build_genesis: impl FnOnce(u64) -> World,
) -> Result<World, Error> {
    Ok(build_genesis(seed_from_log(events)?))
}

/// Fold the WHOLE recorded log through the pure `tick` reducer, event by event,
/// from `genesis`. Every input is present in the log, so the emitted Commands are
/// discarded. The result is the canonical ("live") `World` a replay must reproduce
/// byte-for-byte (Inv 6).
pub fn fold_log(genesis: World, events: &[Event]) -> World {
    let mut world = genesis;
    for ev in events {
        let (next, _commands) = tick(&world, ev);
        world = next;
    }
    world
}

/// Whether a `LogicalInput` is a MODEL-CALL result the `ReplayCursor` stands in
/// for: the `CallModel` duals only. This is the partition a tool-bearing replay
/// loop uses — NARROWER than `effects::is_derived_result` on purpose: while the
/// `RunTool` / `RequestHumanAction` replay drivers are still stubs, the
/// `ToolReturned` / `HumanActionDone` results are re-applied directly by the loop,
/// so a continuation `CallModel` re-emitted after them pulls the NEXT
/// `ModelResponded` from the cursor — never a tool/human result.
pub fn is_model_call_result(input: &LogicalInput) -> bool {
    matches!(
        input,
        LogicalInput::ModelResponded { .. }
            | LogicalInput::ModelFailed { .. }
            | LogicalInput::InferenceCancelled { .. }
    )
}

/// Whether a `LogicalInput` is EXOGENOUS — a free variable the runtime drives the
/// world with, as opposed to a DERIVED effect result the `ReplayCursor` stands in
/// for. This is exactly the complement of `effects::is_derived_result` (kept in
/// lockstep with it): replay re-applies every exogenous input directly and pulls
/// every derived result from the cursor. The exogenous set therefore includes the
/// `SessionStarted` header and the named non-fingerprinted derived exceptions
/// (`ToolAborted`, `ChildReturned`, `HumanActionAborted`), which are in-World
/// identity-correlated results, not shell-dispatched effect results.
pub fn is_exogenous(input: &LogicalInput) -> bool {
    !matches!(
        input,
        LogicalInput::ModelResponded { .. }
            | LogicalInput::ModelFailed { .. }
            | LogicalInput::InferenceCancelled { .. }
            | LogicalInput::ToolReturned { .. }
            | LogicalInput::HumanActionDone { .. }
            | LogicalInput::Compacted { .. }
    )
}

/// Fold the recorded log from `genesis` under the REPLAY driver, reconstructing the
/// `World` with ZERO model calls (Inv 6/7).
///
/// `is_recorded_result` partitions the log: events matching it become the
/// `ReplayCursor`'s recorded results (stood in for each emitted `CallModel` /
/// `Compact`), and every OTHER event is re-applied directly by the loop — exactly
/// as crash-resume re-applies a recorded Input through the reducer. For each tick's
/// Commands, `drive_replay` reuses the next recorded result ONLY while the
/// re-emitted request re-hashes to its `Fingerprint`; a `Diverged` outcome (a
/// fingerprint mismatch or an exhausted cursor) is the fork/edit boundary, which a
/// FAITHFUL replay never reaches, so it is reported as an error here.
pub fn replay_world(
    genesis: World,
    events: &[Event],
    is_recorded_result: impl Fn(&LogicalInput) -> bool,
) -> Result<World, Error> {
    let mut world = genesis;
    // The cursor reads the recorded results in log order; `ReplayCursor::new`
    // additionally filters by `effects::is_derived_result`, a no-op for a predicate
    // (like `is_model_call_result`) whose matches are already a subset of it.
    let recorded: Vec<Event> = events
        .iter()
        .filter(|e| is_recorded_result(&e.input))
        .cloned()
        .collect();
    let mut cursor = ReplayCursor::new(&recorded);

    for ev in events.iter().filter(|e| !is_recorded_result(&e.input)) {
        let (next, mut commands) = tick(&world, ev);
        world = next;

        while !commands.is_empty() {
            let replayed = drive_replay(&commands, &mut cursor)?;
            commands = Vec::new();
            for outcome in replayed {
                match outcome {
                    Replayed::Reused(event) => {
                        let (next, mut cmds) = tick(&world, &event);
                        world = next;
                        commands.append(&mut cmds);
                    }
                    Replayed::Diverged => {
                        return Err(Error::World(
                            "replay diverged: a re-emitted request did not match the \
                             recorded result, but a faithful replay must reuse every result"
                                .into(),
                        ));
                    }
                }
            }
        }
    }

    Ok(world)
}

// ---------------------------------------------------------------------------
// RESTORE — nearest snapshot + tail replay + bounded resume (Inv 5/9/17)
// ---------------------------------------------------------------------------

/// The `cmd`s the reconstructed `World` is still AWAITING — every dispatched
/// effect whose terminal result has not yet folded in. Reading them off the
/// World's `Activity` states (rather than re-scanning the whole log) is what
/// makes resume BOUNDED: the snapshot+tail World already encodes the in-flight
/// effects, including one whose `CommandDispatched` predates the snapshot.
///
/// Scans each entity's `Activity`:
/// - `Thinking { cmd }` / `Compacting { cmd }` — the single in-flight call.
/// - `ResolvingToolUses` — each `Pending { cmd: Some(cmd) }` slot, EXCLUDING the
///   `HELD_CMD` sentinel (a slot held for an approval interaction was never
///   dispatched, so no effect is in flight for it).
/// - `Cancelling { awaiting }` — every cmd still owed an abort ack.
/// - `Idle` — nothing outstanding.
///
/// A deterministic `BTreeSet` (never a `HashMap`) keeps membership free of a
/// hidden ordering input (Inv 8).
///
/// See docs/agent/world/ecs-runtime.md — Restore snapshots; Reconciliation
/// (Inv 5/17).
pub fn outstanding_cmds(world: &World) -> BTreeSet<CmdId> {
    let mut out = BTreeSet::new();
    for components in world.entities.values() {
        match &components.activity {
            Activity::Idle => {}
            Activity::Thinking { cmd } | Activity::Compacting { cmd } => {
                out.insert(*cmd);
            }
            Activity::Cancelling { awaiting } => {
                out.extend(awaiting.iter().copied());
            }
            Activity::ResolvingToolUses { slots } => {
                for slot in slots {
                    if let SlotState::Pending { cmd: Some(cmd) } = slot.state
                        && cmd != HELD_CMD
                    {
                        out.insert(cmd);
                    }
                }
            }
        }
    }
    out
}

/// Bounded resume: reconcile EXACTLY the reconstructed World's `outstanding` set
/// against the recorded dispatch-intents, appending each `Settled` result to
/// `log` at a fresh tick.
///
/// This is the wrapper the restore path layers over `effects::resume`: it first
/// narrows the full `lifecycle` to the `CommandDispatched` records whose `cmd` is
/// in `outstanding` — the bounded set keyed on the World, NOT a full-log rescan —
/// then delegates to `effects::resume`, which derives `next_at = max(events.at)+1`
/// (one Input per tick, Inv 12) and applies the per-`EffectKind` reconciliation
/// policy. Because an awaited `cmd` has, by construction, NO terminal result in
/// `events`, `effects::resume`'s own "skip already-resolved" guard is a no-op on
/// this set, so the result is identical to a resume from genesis over the full
/// lifecycle — only narrowed to the cmds the World proves are still in flight.
///
/// See docs/agent/world/ecs-runtime.md — Reconciliation = the replay→resume edge
/// (Inv 5/17).
pub fn resume_session<L>(
    outstanding: &BTreeSet<CmdId>,
    events: &[Event],
    lifecycle: &[LifecycleEvent],
    log: &mut L,
) -> Result<Vec<Reconciliation>, Error>
where
    L: EventLog,
{
    let mut bounded: Vec<LifecycleEvent> = Vec::new();
    for record in lifecycle {
        if let LifecycleEvent::CommandDispatched { cmd, .. } = record
            && outstanding.contains(cmd)
        {
            bounded.push(record.clone());
        }
    }
    resume(events, &bounded, log)
}

/// Reconstruct the `World` at `target` from the NEAREST valid snapshot ≤ target
/// (else genesis) plus a tail fold, then bounded-resume the in-flight effects.
///
/// 1. **Anchor.** `store.nearest_at_or_before(target)` returns the max-tick valid
///    snapshot ≤ target — the store validates each digest and descends past any
///    corruption, returning `None` (→ genesis) only when none survives, so no data
///    is ever lost (Inv 4/9). `floor` records whether the anchor is a snapshot at
///    tick `t` (fold events with `at > t`) or genesis (`None` → fold the WHOLE
///    prefix): both a genesis World and a tick-0 snapshot read `clock == 0`, but
///    genesis has folded NOTHING while a tick-0 snapshot has already folded the
///    `at == 0` header, so the distinction cannot be collapsed onto `tick`.
/// 2. **Tail.** Fold the events the anchor has not yet absorbed, bounded above by
///    `target`, through the pure `tick` reducer. Folding is ASSOCIATIVE and the
///    snapshot is itself `fold_log(genesis, prefix)`, so this equals folding the
///    whole `events[..=target]` from genesis — provably identical ids/RNG/clock to
///    a full replay (Inv 8/9). `fold_log` takes NO `ModelCaller`, so the tail is
///    reconstructed with ZERO model calls by construction.
/// 3. **Resume.** Reconcile the reconstructed World's [`outstanding_cmds`] against
///    the full `lifecycle` (so a dispatch predating the snapshot still settles),
///    appending each `Settled` result to `log` at a fresh tick — identical to a
///    resume from genesis (Inv 5/17).
///
/// Returns the PRE-resume reconstruction: byte-identical to a full replay from
/// genesis. The synthesised reconciliations are durable in `log` (where a later
/// replay folds them), not folded into the returned World — so the equality with
/// a full replay holds whether or not a crash tail was reconciled.
///
/// `build_genesis` is the SAME genesis constructor the live run used (the world
/// reseeds from the log's `SessionStarted` header, Inv 8). `events`/`lifecycle`
/// are the loaded stratum-1 / stratum-2 streams; `log` is the same log they were
/// loaded from, which the resume phase appends to.
///
/// See docs/agent/world/ecs-runtime.md — Restore snapshots (RESTORE); Inv 5
/// (reconciliation), Inv 8 (recorded seed), Inv 9 (the log is single source).
pub fn restore<L>(
    store: &SnapshotStore,
    events: &[Event],
    lifecycle: &[LifecycleEvent],
    build_genesis: impl FnOnce(u64) -> World,
    target: Tick,
    log: &mut L,
) -> Result<World, Error>
where
    L: EventLog,
{
    let (start_world, floor) = match store.nearest_at_or_before(target)? {
        Some(snapshot) => (snapshot.world, Some(snapshot.tick)),
        None => (genesis_from_log(events, build_genesis)?, None),
    };

    let tail: Vec<Event> = events
        .iter()
        .filter(|e| e.at <= target && floor.is_none_or(|f| e.at > f))
        .cloned()
        .collect();
    let world = fold_log(start_world, &tail);

    let outstanding = outstanding_cmds(&world);
    resume_session(&outstanding, events, lifecycle, log)?;

    Ok(world)
}

/// The single replay-or-live mode `replay_branch` runs under. It flips EXACTLY
/// ONCE, `Replay` → `Live`, at the first divergence and NEVER returns: the
/// one-way handoff is STRUCTURAL — there is no transition back to `Replay`, so a
/// branch can never silently re-enter cached reuse after going live (Inv 6/7).
/// See docs/agent/world/ecs-runtime.md (One-way replay → live handoff).
enum Mode {
    Replay,
    Live,
}

/// The COUNTERFACTUAL branch driver: replay a branch's recorded prefix by
/// fingerprint and, at the FIRST divergence, flip ONE-WAY to live execution for
/// the remainder (Inv 6/7).
///
/// The loop mirrors [`replay_world`] while `Mode::Replay`: it folds each
/// EXOGENOUS branch input and drives the emitted Commands through
/// `effects::drive_replay` against a [`ReplayCursor`] over the branch's recorded
/// derived results, folding each `Reused` recorded result with ZERO model/tool
/// calls (the `client`/`surface_driver` are never reached). The partition between
/// "recorded result" (stood in by the cursor) and "re-applied directly" is the
/// caller-supplied `is_recorded_result`, exactly as in `replay_world`.
///
/// At the FIRST `Diverged` — the edit's first affected `CallModel`, where the
/// re-emitted request no longer re-hashes to the recorded `Fingerprint`, or the
/// cursor is exhausted — it FLIPS `Mode::Live` (one-way) and:
///
///  1. folds the reused prefix `[0..i)` of that batch (the `Reused` results
///     before the boundary), then
///  2. drives the UNPROCESSED remainder `commands[i..]` — `drive_replay` broke at
///     the diverging Command, so it and every later Command in the batch were left
///     undriven — plus the reused prefix's continuations through
///     `effects::drive_live` (fresh, paid results appended to the branch `log`).
///
/// From then on EVERY subsequent tick's Commands are driven live; the cursor is
/// abandoned and never consulted again (no return to replay). The recorded results
/// past the boundary are simply ignored — the branch replaces them with its own
/// live tail.
///
/// A no-op / unchanged branch re-fingerprints identically and reuses the ENTIRE
/// tail with zero live calls, so `replay_branch` over an unchanged branch is
/// byte-identical to `replay_world` over it (`Mode` never leaves `Replay`).
///
/// Generic over the three live-IO capabilities exactly like `effects::drive_live`
/// so it is offline-testable. `genesis` is the branch's reseeded genesis `World`
/// (built via [`genesis_from_log`]); `branch_events` is the branch's recorded log
/// (prefix-by-reference + the edit); `log` is the branch log the live tail is
/// appended to.
///
/// See docs/agent/world/ecs-runtime.md — Content-addressed replay (B4); Inv 6
/// (replay determinism), Inv 7 (the fingerprint).
pub async fn replay_branch<C, S, L>(
    genesis: World,
    branch_events: &[Event],
    is_recorded_result: impl Fn(&LogicalInput) -> bool,
    client: &C,
    surface_driver: &S,
    log: &mut L,
) -> Result<World, Error>
where
    C: ModelCaller,
    S: SurfaceDriver,
    L: EventLog,
{
    let mut world = genesis;

    // The cursor over the branch's recorded derived results, consumed ONLY while
    // `Mode::Replay`. Once we flip to `Live` it is abandoned (fresh results replace
    // it). `ReplayCursor::new` additionally filters by `effects::is_derived_result`,
    // a no-op for a predicate whose matches are already a subset of it.
    let recorded: Vec<Event> = branch_events
        .iter()
        .filter(|e| is_recorded_result(&e.input))
        .cloned()
        .collect();
    let mut cursor = ReplayCursor::new(&recorded);

    // The two edges the live tail's results route on, recovered from the branch's
    // recorded results so the live continuation routes EXACTLY as the original
    // live driver did (conversation results and the agent's surface writes on
    // distinct edges, Inv 19).
    let (conversation_edge, app_edge) = branch_edges(branch_events);

    // The monotonic tick the NEXT appended live Event is stamped at: after the
    // whole recorded branch (one Input per tick, Inv 12).
    let mut next_at: Tick = branch_events
        .iter()
        .map(|e| e.at)
        .max()
        .map_or(0, |m| m + 1);

    // The mode that flips EXACTLY ONCE (replay → live) and never back.
    let mut mode = Mode::Replay;

    for ev in branch_events.iter().filter(|e| !is_recorded_result(&e.input)) {
        let (next, mut commands) = tick(&world, ev);
        world = next;

        // Drive this tick's emitted Commands to quiescence under the CURRENT mode.
        while !commands.is_empty() {
            match mode {
                Mode::Replay => {
                    let replayed = drive_replay(&commands, &mut cursor)?;
                    // `drive_replay` yields `[Reused.., Diverged?]` — a single
                    // `Diverged` only ever as the trailing element. Fold every
                    // `Reused` (re-tick the recorded result, ZERO live calls) and
                    // collect their continuation Commands.
                    let mut continuation = Vec::new();
                    let mut reused_count = 0usize;
                    let mut diverged = false;
                    for outcome in replayed {
                        match outcome {
                            Replayed::Reused(event) => {
                                reused_count += 1;
                                let (next, mut cmds) = tick(&world, &event);
                                world = next;
                                continuation.append(&mut cmds);
                            }
                            Replayed::Diverged => diverged = true,
                        }
                    }

                    if diverged {
                        // FLIP to Live (one-way). `drive_replay` broke at the
                        // diverging Command, so `commands[boundary..]` — that
                        // Command and every later one in the batch — was left
                        // UNPROCESSED. Drive it live AHEAD of the reused prefix's
                        // continuations; the next loop turn drives `commands` live.
                        let boundary = boundary_index(&commands, reused_count);
                        let mut live_commands: Vec<Command> = commands[boundary..].to_vec();
                        live_commands.append(&mut continuation);
                        commands = live_commands;
                        mode = Mode::Live;
                    } else {
                        commands = continuation;
                    }
                }
                Mode::Live => {
                    let stamp = ResultStamp {
                        edge: conversation_edge,
                        app_edge,
                        // A fresh, evaluation-reproducible branch: record no host
                        // clock reading. `drive_live` leaves `Resources.wall`
                        // untouched on a `None` wall.
                        wall: None,
                        at: next_at,
                    };
                    next_at += 1;

                    let results = drive_live(
                        &commands,
                        stamp,
                        &world.resources.surfaces,
                        client,
                        surface_driver,
                        log,
                    )
                    .await?;

                    commands = Vec::new();
                    for result in &results {
                        let (next, mut cmds) = tick(&world, result);
                        world = next;
                        commands.append(&mut cmds);
                    }
                }
            }
        }
    }

    Ok(world)
}

/// The index in `commands` of the Command where `drive_replay` diverged: the
/// `reused_count`-th fingerprinted (`CallModel` / `Compact`) Command, 0-based —
/// i.e. the FIRST fingerprinted Command past the `reused_count` already-reused
/// ones. `drive_replay` breaks at that Command, so `commands[boundary..]` is the
/// UNPROCESSED remainder the live driver takes over.
fn boundary_index(commands: &[Command], reused_count: usize) -> usize {
    let mut seen = 0usize;
    for (idx, command) in commands.iter().enumerate() {
        if matches!(command, Command::CallModel { .. } | Command::Compact { .. }) {
            if seen == reused_count {
                return idx;
            }
            seen += 1;
        }
    }
    commands.len()
}

/// The two edges the live tail's results route on, recovered from the branch's
/// recorded results so a live continuation routes EXACTLY as the original live
/// driver did: conversation results (model calls / compaction) on the edge they
/// were recorded on, and the agent's surface writes on the DISTINCT app edge
/// (Inv 19). When the branch carries no such recorded result yet, fall back to the
/// runtime's edge convention (conversation edge `0`, app edge `1`).
fn branch_edges(events: &[Event]) -> (EdgeId, EdgeId) {
    let conversation = events
        .iter()
        .find(|e| {
            matches!(
                e.input,
                LogicalInput::ModelResponded { .. }
                    | LogicalInput::ModelFailed { .. }
                    | LogicalInput::InferenceCancelled { .. }
                    | LogicalInput::Compacted { .. }
            )
        })
        .map_or(0, |e| e.edge);
    let app = events
        .iter()
        .find(|e| matches!(e.input, LogicalInput::ToolReturned { .. }))
        .map_or(1, |e| e.edge);
    (conversation, app)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::agent::world::budget::Budget;
    use crate::agent::world::effects::{fingerprint_call, Command};
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{Block, History};
    use crate::agent::world::inputs::{
        Capabilities, Fingerprint, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources,
    };

    const SEED: u64 = 7;

    fn offline_model() -> ModelConfig {
        ModelConfig {
            model: "claude-replay-spine-unit".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// The fresh `World` a session starts from: tick 0, a single primary `Idle`
    /// root entity, `Resources` reseeded from `seed`. Mirrors the genesis every
    /// replay harness builds (the shell owns genesis; no P0 System creates it).
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
                model: None,
            },
        );
        world
    }

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

    fn model_responded(at: u64, cmd: CmdId, text: &str) -> Event {
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
                    model_id: "claude-replay-spine-unit".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    /// Stamp each model-call result's `fingerprint` with the hash the LIVE driver
    /// would record for the request the reducer re-emits for that `cmd`, so the
    /// replay REUSES each result instead of diverging (Inv 7).
    fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig) {
        let mut world = genesis_from_log(events, |seed| genesis(seed, model))
            .expect("genesis for the fingerprint pass");
        let mut by_cmd: BTreeMap<CmdId, Fingerprint> = BTreeMap::new();
        for ev in events.iter() {
            let (next, commands) = tick(&world, ev);
            world = next;
            for command in &commands {
                if let Command::CallModel {
                    cmd,
                    messages,
                    tools,
                    params,
                    ..
                } = command
                {
                    let fp =
                        fingerprint_call(messages, tools, params).expect("fingerprint a request");
                    by_cmd.insert(*cmd, fp);
                }
            }
        }
        for ev in events.iter_mut() {
            if let LogicalInput::ModelResponded {
                cmd, fingerprint, ..
            } = &mut ev.input
                && let Some(fp) = by_cmd.get(cmd)
            {
                *fingerprint = fp.clone();
            }
        }
    }

    /// VC-2.2: a folded log and a cursor-driven replay of the SAME log reconstruct
    /// a BYTE-IDENTICAL `World`, and the replay never diverges (it reuses every
    /// recorded model-call result).
    #[test]
    fn fold_and_replay_are_byte_identical() {
        let model = offline_model();
        let mut events = vec![
            session_started(),
            user_message(1, "say hi"),
            model_responded(2, 0, "hi"),
        ];
        stamp_fingerprints(&mut events, &model);

        let live = fold_log(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
        );
        let replay = replay_world(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
            is_model_call_result,
        )
        .expect("a faithful replay reuses every recorded result, never diverging");

        assert_eq!(
            serde_json::to_vec(&live).expect("serialize the folded World"),
            serde_json::to_vec(&replay).expect("serialize the replayed World"),
            "fold_log and replay_world must reconstruct a BYTE-IDENTICAL World (Inv 6)"
        );
        assert_eq!(live, replay, "the folded and replayed Worlds are equal");
    }

    /// `is_exogenous` is the exact complement of the derived-result set the cursor
    /// stands in for, and `is_model_call_result` is the narrower model-call subset.
    #[test]
    fn classifiers_partition_the_log() {
        let derived = [
            LogicalInput::ModelResponded {
                cmd: 0,
                entity: 0,
                fingerprint: Fingerprint(String::new()),
                blocks: Vec::new(),
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: String::new(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
            LogicalInput::ToolReturned {
                cmd: 0,
                entity: 0,
                fingerprint: Fingerprint(String::new()),
                result: Vec::new(),
            },
        ];
        for input in &derived {
            assert!(!is_exogenous(input), "a derived result is not exogenous");
        }
        // A tool result is derived (not exogenous) yet is NOT a model-call result —
        // the gap that forces the narrower partition for tool-bearing logs.
        assert!(!is_model_call_result(&derived[1]));

        let exogenous = [
            LogicalInput::SessionStarted {
                seed: SEED,
                surface_tools: Vec::new(),
            },
            LogicalInput::UserMessage {
                to: 0,
                text: String::new(),
            },
        ];
        for input in &exogenous {
            assert!(is_exogenous(input), "a free variable is exogenous");
            assert!(!is_model_call_result(input));
        }
    }

    // --- CR-branch-replay: the counterfactual replay → live boundary -----------

    use crate::agent::world::event_log::{EventLog, MemoryEventLog};
    use serde_json::Value as Json;

    /// A `ModelCaller` that PANICS if invoked, proving the replay path makes ZERO
    /// live calls: an unchanged branch reuses the whole recorded tail and never
    /// flips to live, so this client is structurally unreachable.
    struct ExplodingClient;

    impl ModelCaller for ExplodingClient {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            panic!("an unchanged branch must never invoke the model client");
        }
    }

    /// A `ModelCaller` that returns a fixed `EndTurn` assistant text and COUNTS its
    /// invocations, so a test proves the live client fires only PAST the divergence
    /// boundary — and exactly once.
    struct CountingClient {
        text: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingClient {
        fn new(text: &str) -> Self {
            Self {
                text: text.into(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl ModelCaller for CountingClient {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                vec![Block::Text {
                    text: self.text.clone(),
                }],
                ModelMeta {
                    usage: Usage::default(),
                    model_id: "claude-replay-spine-unit".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            ))
        }
    }

    /// A `SurfaceDriver` that PANICS if reached: the model-only branch tests never
    /// drive a surface, so a `CallModel` path proves the sink is untouched.
    struct NoSurfaceDrive;

    impl SurfaceDriver for NoSurfaceDrive {
        async fn drive(&self, _command: &Command) -> Result<(), Error> {
            panic!("a CallModel-only branch must not reach the surface driver");
        }
    }

    /// VC-1.1 / VC-1.3: an UNCHANGED branch re-fingerprints identically and reuses
    /// the ENTIRE recorded tail with ZERO live calls — the `ExplodingClient` is
    /// structurally unreachable (no Command ever diverges, so `Mode` never leaves
    /// `Replay`). The branch World is byte-identical to `fold_log`/`replay_world`,
    /// and nothing is appended to the branch log.
    #[tokio::test]
    async fn unchanged_branch_reuses_the_entire_tail_with_zero_live_calls() {
        let model = offline_model();
        let mut events = vec![
            session_started(),
            user_message(1, "say hi"),
            model_responded(2, 0, "hi"),
        ];
        stamp_fingerprints(&mut events, &model);

        // The canonical faithful replay (and fold) the unchanged branch must
        // reproduce.
        let folded = fold_log(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
        );
        let replayed = replay_world(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
            is_model_call_result,
        )
        .expect("a faithful replay reuses every recorded result");

        let mut log = MemoryEventLog::new();
        let branch = replay_branch(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
            is_model_call_result,
            &ExplodingClient,
            &NoSurfaceDrive,
            &mut log,
        )
        .await
        .expect("an unchanged branch reuses the whole tail with zero live calls");

        assert_eq!(
            serde_json::to_vec(&branch).expect("serialize the branch World"),
            serde_json::to_vec(&folded).expect("serialize the folded World"),
            "an unchanged branch is byte-identical to fold_log (Inv 6)"
        );
        assert_eq!(
            branch, replayed,
            "replay_branch over an unchanged branch == replay_world"
        );
        assert!(
            log.load().expect("load the branch log").is_empty(),
            "zero live results appended — the entire tail was reused"
        );
    }

    /// VC-1.1 / VC-1.2: a branch whose turn-2 EXOGENOUS input is edited (so its
    /// recorded turn-2 result re-fingerprints differently) flips `Replay → Live`
    /// EXACTLY ONCE at turn 2: turn 1 reuses its recorded result with ZERO calls,
    /// then at the first `Diverged` the live `client` fires (only past the boundary)
    /// and its fresh result is appended to the BRANCH log. The branch World diverges
    /// from the original from the edit onward.
    #[tokio::test]
    async fn edited_prefix_flips_to_live_exactly_once_at_the_divergence() {
        let model = offline_model();

        // A two-turn ORIGINAL log: turn 1 (cmd 0) and turn 2 (cmd 1). Fingerprints
        // are stamped for the UNEDITED requests so a faithful replay reuses both.
        let mut original = vec![
            session_started(),
            user_message(1, "first question"),
            model_responded(2, 0, "first answer"),
            user_message(3, "second question"),
            model_responded(4, 1, "second answer"),
        ];
        stamp_fingerprints(&mut original, &model);

        // The branch EDITS the turn-2 exogenous input but KEEPS the recorded results
        // with their ORIGINAL fingerprints. Turn 1 is unchanged → its recorded
        // result still matches; turn 2's request now differs → its recorded result
        // is STALE → the first (and only) divergence is exactly at turn 2.
        let mut branch = original.clone();
        match &mut branch[3].input {
            LogicalInput::UserMessage { text, .. } => *text = "second question (edited)".into(),
            _ => panic!("branch[3] must be the turn-2 user message"),
        }

        let client = CountingClient::new("live answer");
        let mut log = MemoryEventLog::new();
        let branch_world = replay_branch(
            genesis_from_log(&branch, |seed| genesis(seed, &model)).expect("genesis"),
            &branch,
            is_model_call_result,
            &client,
            &NoSurfaceDrive,
            &mut log,
        )
        .await
        .expect("the branch replays the reused prefix then goes live");

        // The live client fired EXACTLY ONCE — only AFTER the boundary (turn 2).
        // Turn 1 reused its recorded result with ZERO calls (one-way flip).
        assert_eq!(
            client.calls(),
            1,
            "the client is invoked only past the divergence, never for the reused turn 1"
        );

        // The fresh live result was appended to the BRANCH log (the divergent tail).
        let appended = log.load().expect("load the branch log");
        assert_eq!(appended.len(), 1, "exactly one fresh live result is appended");
        assert!(
            matches!(
                &appended[0].input,
                LogicalInput::ModelResponded { blocks, .. }
                    if matches!(blocks.as_slice(), [Block::Text { text }] if text == "live answer")
            ),
            "the appended result is the fresh live model response"
        );

        // The branch World diverges from the original (a faithful replay of the
        // UNEDITED log) from the edit onward.
        let original_world = replay_world(
            genesis_from_log(&original, |seed| genesis(seed, &model)).expect("genesis"),
            &original,
            is_model_call_result,
        )
        .expect("the unedited log replays faithfully");
        assert_ne!(
            serde_json::to_vec(&branch_world).expect("serialize branch World"),
            serde_json::to_vec(&original_world).expect("serialize original World"),
            "the branch World diverges from the original from the edit onward"
        );
    }

    // --- CR-restore: nearest snapshot + tail replay + bounded resume ----------

    use crate::agent::world::lifecycle::{ActorCtx, AppId, EffectKind, IdempotencyKey};
    use crate::agent::world::snapshot::snapshot_at;

    /// VC-2.2: `restore(target)` reconstructs the World from the nearest snapshot ≤
    /// target plus a tail fold; for EVERY target it is byte-identical to a full
    /// replay from genesis — proving folding's associativity (a snapshot is a
    /// fold-prefix) with ZERO model calls. A complete log has nothing outstanding,
    /// so the resume phase appends nothing.
    #[tokio::test]
    async fn restore_at_various_ticks_equals_full_replay() {
        let model = offline_model();
        let mut events = vec![
            session_started(),
            user_message(1, "first question"),
            model_responded(2, 0, "first answer"),
            user_message(3, "second question"),
            model_responded(4, 1, "second answer"),
        ];
        stamp_fingerprints(&mut events, &model);

        // Seed a store with snapshots at ticks 0 and 2 so different targets resolve
        // to different anchors (tick 0, tick 2, or tail-only past tick 2).
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        for t in [0u64, 2] {
            let snap = snapshot_at(&events, |seed| genesis(seed, &model), t).expect("snapshot_at");
            store.write(&snap).expect("write snapshot");
        }

        // A complete log: no outstanding effects, so the lifecycle carries no
        // dispatch-intents and resume is a no-op.
        let lifecycle: Vec<LifecycleEvent> = Vec::new();

        for target in 0u64..=4 {
            let prefix: Vec<Event> = events.iter().filter(|e| e.at <= target).cloned().collect();
            let full = fold_log(
                genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
                &prefix,
            );

            let mut log = MemoryEventLog::new();
            let restored = restore(
                &store,
                &events,
                &lifecycle,
                |seed| genesis(seed, &model),
                target,
                &mut log,
            )
            .expect("restore");

            assert_eq!(
                serde_json::to_vec(&restored).expect("serialize restored"),
                serde_json::to_vec(&full).expect("serialize full replay"),
                "restore({target}) must be byte-identical to a full replay from genesis"
            );
            assert!(
                log.load().expect("load resume log").is_empty(),
                "a complete log has nothing to reconcile at target {target}"
            );
        }

        // Cross-check the final target against the cursor-driven replay spine too
        // (fold_log ≡ replay_world for a faithful log), so "full replay" is proven
        // on BOTH reconstruction primitives.
        let replayed = replay_world(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
            is_model_call_result,
        )
        .expect("a faithful log replays without diverging");
        let mut log = MemoryEventLog::new();
        let restored_full = restore(
            &store,
            &events,
            &lifecycle,
            |seed| genesis(seed, &model),
            4,
            &mut log,
        )
        .expect("restore at the final tick");
        assert_eq!(
            serde_json::to_vec(&restored_full).expect("serialize"),
            serde_json::to_vec(&replayed).expect("serialize"),
            "restore at the final tick equals replay_world over the whole log"
        );
    }

    /// VC-2.2 (fallback): with NO snapshots the store returns `None`, so restore
    /// falls back to genesis and folds the WHOLE prefix — still byte-identical to a
    /// full replay (no data loss when no snapshot survives, Inv 9).
    #[tokio::test]
    async fn restore_falls_back_to_genesis_without_snapshots() {
        let model = offline_model();
        let mut events = vec![
            session_started(),
            user_message(1, "q"),
            model_responded(2, 0, "a"),
        ];
        stamp_fingerprints(&mut events, &model);

        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots")); // never written
        let lifecycle: Vec<LifecycleEvent> = Vec::new();

        let mut log = MemoryEventLog::new();
        let restored = restore(
            &store,
            &events,
            &lifecycle,
            |seed| genesis(seed, &model),
            2,
            &mut log,
        )
        .expect("restore with no snapshots falls back to genesis");

        let full = fold_log(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
        );
        assert_eq!(
            serde_json::to_vec(&restored).expect("serialize"),
            serde_json::to_vec(&full).expect("serialize"),
            "restore with an empty store equals a full replay from genesis"
        );
    }

    /// VC-2.4: a crash tail — an outstanding `CommandDispatched { CallModel }` whose
    /// dispatch (tick 1) PREDATES the snapshot (tick 2) and whose `ModelResponded`
    /// was never logged. restore reconstructs the still-`Thinking` World, reads the
    /// in-flight cmd off its `Activity`, and reconciles it from the FULL lifecycle —
    /// byte-identically to a resume from genesis, including the appended `Settled`
    /// `InferenceCancelled { Crash }`. The returned World is the pre-resume
    /// reconstruction (equal to a full replay over the same truncated log).
    #[tokio::test]
    async fn crash_tail_restore_reconciles_like_genesis_resume() {
        let model = offline_model();
        // Turn 1 leaves the entity `Thinking`; a second mid-run message parks in the
        // Inbox and advances the clock WITHOUT resolving the call — so the dispatch
        // at tick 1 strictly predates the tick-2 snapshot, and the call is the crash
        // tail (no `ModelResponded` is ever logged).
        let events = vec![
            session_started(),
            user_message(1, "thinking…"),
            user_message(2, "are you there?"),
        ];

        // Reconstruct to discover the minted in-flight cmd from the Thinking state.
        let world = fold_log(
            genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
            &events,
        );
        let cmd = match &world.entities.get(&0).expect("primary entity").activity {
            Activity::Thinking { cmd } => *cmd,
            other => panic!("the primary entity must be Thinking after a crash, got {other:?}"),
        };

        // The write-ahead dispatch-intent for the crashed call (recorded at tick 1).
        let lifecycle = vec![LifecycleEvent::CommandDispatched {
            at: 1,
            cmd,
            kind: EffectKind::CallModel,
            ctx: ActorCtx {
                entity: 0,
                origin: Origin::Agent,
                edge: 0,
            },
            key: IdempotencyKey {
                app_id: AppId::default(),
                tick: 1,
                effect_id: 0,
            },
            fingerprint: Fingerprint(format!("fp-{cmd}")),
        }];

        // The canonical reconciliation: a resume from genesis over the full streams.
        let mut genesis_log = MemoryEventLog::new();
        let genesis_recon =
            resume(&events, &lifecycle, &mut genesis_log).expect("resume from genesis");

        // The bounded resume keyed on the World's outstanding set reconciles
        // IDENTICALLY (same Vec<Reconciliation>, same appended Settled events).
        let outstanding = outstanding_cmds(&world);
        assert_eq!(
            outstanding,
            BTreeSet::from([cmd]),
            "exactly the in-flight cmd is outstanding"
        );
        let mut session_log = MemoryEventLog::new();
        let session_recon = resume_session(&outstanding, &events, &lifecycle, &mut session_log)
            .expect("bounded resume over the outstanding set");
        assert_eq!(
            session_recon, genesis_recon,
            "bounded resume reconciles identically to a resume from genesis"
        );
        assert_eq!(
            session_log.load().expect("load session log"),
            genesis_log.load().expect("load genesis log"),
            "bounded resume appends the same Settled events as genesis resume"
        );

        // restore: anchor on a tick-2 snapshot (entity Thinking) with an empty tail;
        // the dispatch predating the snapshot is reconciled from the FULL lifecycle
        // keyed on the reconstructed World's outstanding set.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let snap = snapshot_at(&events, |seed| genesis(seed, &model), 2).expect("snapshot at 2");
        assert_eq!(snap.tick, 2, "the snapshot anchors AFTER the dispatch at tick 1");
        store.write(&snap).expect("write snapshot");

        let mut restore_log = MemoryEventLog::new();
        let restored = restore(
            &store,
            &events,
            &lifecycle,
            |seed| genesis(seed, &model),
            2,
            &mut restore_log,
        )
        .expect("restore over a crash tail");

        // The returned World is the PRE-resume reconstruction (still Thinking) —
        // byte-identical to a full replay over the same crash-truncated log.
        assert_eq!(
            serde_json::to_vec(&restored).expect("serialize restored"),
            serde_json::to_vec(&world).expect("serialize full replay"),
            "restore returns the pre-resume reconstruction, equal to a full replay"
        );
        // The reconciliation restore appended matches a genesis resume exactly.
        assert_eq!(
            restore_log.load().expect("load restore log"),
            genesis_log.load().expect("load genesis log"),
            "restore reconciles the predating crash dispatch identically to genesis resume"
        );
    }
}
