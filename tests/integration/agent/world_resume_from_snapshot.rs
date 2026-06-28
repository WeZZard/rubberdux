//! VC-2.4 — restore + bounded-resume over a crash tail (offline).
//!
//! This is the integration sink for the CRASH-RESUME property of the restore path.
//! It drives `restore` over a synthetic crash tail and proves that bounded-resume
//! reconciles the outstanding in-flight commands IDENTICALLY to a resume from
//! genesis — even when the `CommandDispatched` intent PREDATES the snapshot tick.
//!
//! - **VC-2.4 (Edge).** A session where a `CallModel` dispatch (tick 1) survived
//!   the crash in the stratum-2 WAL but its result `Event` was never logged: after
//!   writing a snapshot at tick 2 (PAST the dispatch, entity still `Thinking`),
//!   `restore(target=2)` reconstructs the `Thinking` World byte-identically to a
//!   full fold, then bounded-`resume_session` reconciles the outstanding cmd from
//!   the FULL lifecycle — appending a `Settled InferenceCancelled{Crash}` whose
//!   fields (`cmd`, `entity`, `fingerprint`, `reason`) match a `resume` from
//!   genesis byte-for-byte. (Inv 5, 10, 17.)
//!
//!   Two variants prove the same property against DIFFERENT snapshot anchors:
//!   - **Snapshot AFTER the dispatch** (tick 2 snapshot, dispatch at tick 1) —
//!     the outstanding cmd predates the snapshot; the restore must still find it
//!     via `outstanding_cmds(&world)` and reconcile from the full lifecycle.
//!   - **No snapshot** (empty store) — restore falls back to genesis + full tail;
//!     bounded-resume produces the identical reconciliation. Proves the fallback
//!     path is consistent with the snapshot path (Inv 9).
//!
//! All proofs run OFFLINE: no model client, no network calls, no credentials.
//!
//! See docs/agent/world/ecs-runtime.md — Reconciliation (Inv 5/17), totality
//! (Inv 10), restore snapshots (RESTORE).

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{fingerprint_call, resume, Command, Reconciliation};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::History;
use rubberdux::agent::world::inputs::{CancelReason, Event, Fingerprint, LogicalInput, Origin};
use rubberdux::agent::world::lifecycle::{ActorCtx, AppId, EffectKind, IdempotencyKey, LifecycleEvent};
use rubberdux::agent::world::replay::{
    fold_log, genesis_from_log, outstanding_cmds, restore, resume_session,
};
use rubberdux::agent::world::snapshot::{snapshot_at, SnapshotStore};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};

// ---------------------------------------------------------------------------
// Shared constants and genesis builder
// ---------------------------------------------------------------------------

/// The hidden RNG seed crossing the recorded boundary (Inv 8).
const SEED: u64 = 13;

fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-resume-from-snapshot-sink".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// The fresh `World` a session starts from: tick 0, one primary `Idle` entity,
/// `Resources` reseeded from `seed`. Mirrors the canonical genesis the harness uses.
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
            autonomy: None,
        },
    );
    world
}

// ---------------------------------------------------------------------------
// Log and lifecycle construction helpers
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

/// Build the crash-tail log: `SessionStarted` + `UserMessage` (dispatches a
/// `CallModel`, entity enters `Thinking`) + a second `UserMessage` that parks in
/// the Inbox without resolving the in-flight call. The second message advances the
/// clock to tick 2 so a snapshot can be written AFTER the dispatch.
///
/// Tick layout:
/// - 0: `SessionStarted`
/// - 1: `UserMessage("thinking…")` — emits `CallModel cmd X` → entity `Thinking{X}`
/// - 2: `UserMessage("are you there?")` — parks in Inbox; entity still `Thinking{X}`
fn build_crash_log() -> Vec<Event> {
    vec![
        session_started(),
        user_message(1, "thinking…"),
        user_message(2, "are you there?"),
    ]
}

/// Reconstruct the in-flight `CmdId` and its request fingerprint by folding the log
/// and inspecting the Commands the reducer emits for the first `UserMessage`. Returns
/// `(cmd, fingerprint)`.
fn discover_inflight_cmd(events: &[Event], model: &ModelConfig) -> (CmdId, Fingerprint) {
    let mut world =
        genesis_from_log(events, |seed| genesis(seed, model)).expect("genesis from log");
    for ev in events {
        let (next, commands) = tick(&world, ev);
        world = next;
        for command in commands {
            if let Command::CallModel {
                cmd, messages, tools, params, ..
            } = command
            {
                let fp = fingerprint_call(&messages, &tools, &params)
                    .expect("fingerprint the in-flight CallModel request");
                return (cmd, fp);
            }
        }
    }
    panic!("no CallModel was emitted; the crash log must have at least one UserMessage");
}

/// Build the stratum-2 lifecycle for the crash: one `CommandDispatched` at `at`
/// for the in-flight `cmd` carrying its request `fingerprint`. There is NO
/// corresponding result Event in the stratum-1 log — that is the crash condition.
fn build_lifecycle(cmd: CmdId, dispatch_at: u64, fingerprint: Fingerprint) -> Vec<LifecycleEvent> {
    vec![LifecycleEvent::CommandDispatched {
        at: dispatch_at,
        cmd,
        kind: EffectKind::CallModel,
        ctx: ActorCtx {
            entity: 0,
            origin: Origin::Agent,
            edge: 0,
        },
        key: IdempotencyKey {
            app_id: AppId::default(),
            tick: dispatch_at,
            effect_id: 0,
        },
        fingerprint,
    }]
}

// ---------------------------------------------------------------------------
// VC-2.4 helpers — assertion utilities
// ---------------------------------------------------------------------------

/// Assert that `reconciled` is exactly one `Settled InferenceCancelled{Crash}` for
/// `cmd` / entity 0, with envelope `origin = Agent`.
fn assert_exactly_one_crash_settled(reconciled: &[Reconciliation], cmd: CmdId) {
    assert_eq!(
        reconciled.len(),
        1,
        "exactly one outstanding cmd must be reconciled; got {}",
        reconciled.len()
    );
    let Reconciliation::Settled(event) = &reconciled[0] else {
        panic!(
            "expected a Settled reconciliation, got {:?}",
            reconciled[0]
        );
    };
    assert_eq!(
        event.origin,
        Origin::Agent,
        "Settled envelope origin must be Agent (inherited from dispatch ctx, Inv 17)"
    );
    match &event.input {
        LogicalInput::InferenceCancelled {
            cmd: settled_cmd,
            entity,
            reason,
            partial,
            ..
        } => {
            assert_eq!(*settled_cmd, cmd, "settled cmd must match the dispatched cmd");
            assert_eq!(*entity, 0, "settled entity must be 0 (from dispatch ctx)");
            assert_eq!(
                *reason,
                CancelReason::Crash,
                "reason must be Crash (the call was never resolved)"
            );
            assert_eq!(*partial, None, "a crash mid-Thinking carries no partial");
        }
        other => panic!("expected InferenceCancelled, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// VC-2.4 — snapshot AFTER the dispatch; dispatch predates the snapshot tick
// ---------------------------------------------------------------------------

/// **VC-2.4 (primary)** The dispatch at tick 1 PREDATES the snapshot at tick 2.
///
/// After `restore(target=2)`:
/// 1. The returned `World` is `Thinking{cmd}` — byte-identical to a full fold
///    from genesis over the same crash-truncated log.
/// 2. `outstanding_cmds(&world)` contains exactly `{cmd}`.
/// 3. `resume_session` over that outstanding set appends a `Settled
///    InferenceCancelled{Crash}` whose stratum-1 events match a `resume` from
///    genesis byte-for-byte (even though the dispatch predates the snapshot tick).
#[test]
fn vc_2_4_snapshot_after_dispatch_reconciles_crash_tail_like_genesis_resume() {
    let model = offline_model();
    let events = build_crash_log();
    let (cmd, fingerprint) = discover_inflight_cmd(&events, &model);

    // Verify the entity is Thinking after folding (pre-snapshot state).
    let full_world = fold_log(
        genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
        &events,
    );
    assert!(
        matches!(
            full_world.entities.get(&0).expect("entity").activity,
            Activity::Thinking { cmd: c } if c == cmd
        ),
        "the primary entity must be Thinking on the dispatched cmd after a crash"
    );

    // Dispatch at tick 1 (predates the tick-2 snapshot).
    let lifecycle = build_lifecycle(cmd, 1, fingerprint);

    // The log is truly truncated: no result Event for the dispatched cmd.
    assert!(
        !events.iter().any(|e| matches!(
            e.input,
            LogicalInput::ModelResponded { .. }
                | LogicalInput::InferenceCancelled { .. }
        )),
        "the crash log must contain no terminal result for the dispatched cmd"
    );

    // Genesis-resume baseline: resume the full log from genesis, no snapshot.
    let mut genesis_log = MemoryEventLog::new();
    let genesis_recon = resume(&events, &lifecycle, &mut genesis_log)
        .expect("genesis-resume must succeed");
    assert_exactly_one_crash_settled(&genesis_recon, cmd);

    // Write a snapshot at tick 2 (PAST the dispatch at tick 1).
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let snap = snapshot_at(&events, |seed| genesis(seed, &model), 2)
        .expect("snapshot_at(2) must succeed");
    assert_eq!(snap.tick, 2, "snapshot is at tick 2 (AFTER the dispatch at tick 1)");
    store.write(&snap).expect("write snapshot");

    // restore: anchor on the tick-2 snapshot; the dispatch predating it must still
    // be reconciled from the full lifecycle keyed on the reconstructed World's
    // outstanding set.
    let mut restore_log = MemoryEventLog::new();
    let restored = restore(
        &store,
        &events,
        &lifecycle,
        |seed| genesis(seed, &model),
        2,
        &mut restore_log,
    )
    .expect("restore over a crash tail must succeed");

    // The returned World is the PRE-resume reconstruction: byte-identical to the
    // full fold (still Thinking, not yet settled by resume). (Inv 9.)
    assert_eq!(
        serde_json::to_vec(&restored).expect("serialize restored"),
        serde_json::to_vec(&full_world).expect("serialize full fold"),
        "restore must return the pre-resume World (still Thinking), byte-identical to full fold"
    );

    // outstanding_cmds reads the in-flight cmd off the reconstructed World's Activity.
    let outstanding = outstanding_cmds(&restored);
    assert_eq!(
        outstanding,
        std::collections::BTreeSet::from([cmd]),
        "the reconstructed World must show exactly {cmd} as outstanding"
    );

    // The reconciliation restore appended to restore_log must match genesis resume.
    let restore_appended = restore_log.load().expect("load restore log");
    let genesis_appended = genesis_log.load().expect("load genesis log");

    assert_eq!(
        restore_appended, genesis_appended,
        "restore must append the same Settled events as a genesis resume \
         (dispatch predating the snapshot is reconciled from the full lifecycle)"
    );

    // Verify the appended Settled event has the expected structure.
    assert_eq!(
        restore_appended.len(),
        1,
        "exactly one Settled InferenceCancelled must be appended"
    );
    assert!(
        matches!(
            &restore_appended[0].input,
            LogicalInput::InferenceCancelled {
                cmd: c,
                reason: CancelReason::Crash,
                ..
            } if *c == cmd
        ),
        "the appended event must be InferenceCancelled{{Crash}} for the dispatched cmd"
    );
}

// ---------------------------------------------------------------------------
// VC-2.4 — bounded resume matches genesis resume; no-snapshot fallback
// ---------------------------------------------------------------------------

/// **VC-2.4 (no-snapshot path)** With an EMPTY store, `restore` falls back to
/// genesis + full tail before bounded-resume. The reconciliation must be identical
/// to a `resume` from genesis over the full streams — proving the no-snapshot path
/// is consistent with the snapshot path (Inv 9).
#[test]
fn vc_2_4_no_snapshot_fallback_reconciles_identically_to_genesis_resume() {
    let model = offline_model();
    let events = build_crash_log();
    let (cmd, fingerprint) = discover_inflight_cmd(&events, &model);
    let lifecycle = build_lifecycle(cmd, 1, fingerprint);

    // Genesis-resume baseline.
    let mut genesis_log = MemoryEventLog::new();
    let genesis_recon = resume(&events, &lifecycle, &mut genesis_log)
        .expect("genesis-resume must succeed");
    assert_exactly_one_crash_settled(&genesis_recon, cmd);

    // Empty store: restore always falls back to genesis.
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = SnapshotStore::new(dir.path().join("snapshots")); // never written

    let mut restore_log = MemoryEventLog::new();
    let restored = restore(
        &store,
        &events,
        &lifecycle,
        |seed| genesis(seed, &model),
        2,
        &mut restore_log,
    )
    .expect("restore with empty store must succeed via genesis fallback");

    // The full fold: restore with no snapshots is the same as a full fold.
    let full_world = fold_log(
        genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
        &events,
    );
    assert_eq!(
        serde_json::to_vec(&restored).expect("serialize"),
        serde_json::to_vec(&full_world).expect("serialize"),
        "restore with no snapshots must equal full fold from genesis"
    );

    // The appended reconciliation must match genesis resume.
    assert_eq!(
        restore_log.load().expect("load restore log"),
        genesis_log.load().expect("load genesis log"),
        "restore with no snapshots must append the same Settled events as genesis resume"
    );
}

// ---------------------------------------------------------------------------
// VC-2.4 — resume_session matches effects::resume on the full lifecycle
// ---------------------------------------------------------------------------

/// **VC-2.4 (API equivalence)** `resume_session(outstanding, events, lifecycle)`
/// and `effects::resume(events, bounded_lifecycle)` produce IDENTICAL
/// `Vec<Reconciliation>` and append the SAME stratum-1 events to their respective
/// logs. This proves that the bounded public API is a transparent wrapper over the
/// lower-level `resume` — it narrows the lifecycle to the outstanding set and
/// delegates, without changing the reconciliation output (Inv 5/17).
#[test]
fn vc_2_4_resume_session_matches_effects_resume_on_full_lifecycle() {
    let model = offline_model();
    let events = build_crash_log();
    let (cmd, fingerprint) = discover_inflight_cmd(&events, &model);
    let lifecycle = build_lifecycle(cmd, 1, fingerprint);

    // Full fold to get the in-flight outstanding set.
    let world = fold_log(
        genesis_from_log(&events, |seed| genesis(seed, &model)).expect("genesis"),
        &events,
    );
    let outstanding = outstanding_cmds(&world);
    assert_eq!(
        outstanding,
        std::collections::BTreeSet::from([cmd]),
        "exactly one cmd is outstanding"
    );

    // effects::resume over the full lifecycle (the baseline).
    let mut effects_log = MemoryEventLog::new();
    let effects_recon = resume(&events, &lifecycle, &mut effects_log)
        .expect("effects::resume must succeed");

    // resume_session bounded to the outstanding set.
    let mut session_log = MemoryEventLog::new();
    let session_recon = resume_session(&outstanding, &events, &lifecycle, &mut session_log)
        .expect("resume_session must succeed");

    assert_eq!(
        session_recon, effects_recon,
        "resume_session must produce the same Vec<Reconciliation> as effects::resume"
    );
    assert_eq!(
        session_log.load().expect("load session log"),
        effects_log.load().expect("load effects log"),
        "resume_session must append the same Settled events as effects::resume"
    );

    // Verify structural correctness of the single Settled event.
    assert_exactly_one_crash_settled(&session_recon, cmd);
}
