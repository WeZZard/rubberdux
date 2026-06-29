//! VC-1.1 / VC-1.3 / VC-1.4 — the counterfactual replay boundary, OFFLINE.
//!
//! This is the headline milestone sink's OFFLINE half: it drives the PUBLIC
//! branch + replay API end-to-end (`branch::fork` + `replay::replay_branch`) over
//! a hand-authored recorded session and proves the three deterministic boundary
//! properties without a single model call. Each proof threads an
//! [`ExplodingClient`] — a `ModelCaller` that PANICS on any invocation — so "zero
//! model calls" is a STRUCTURAL guarantee: were the replay ever to cross the
//! divergence into live execution, the client would fire and the test would
//! abort. The live half (the actual Replay→Live flip past the edit) is proven by
//! `tests/system/app/counterfactual_branch_live.rs` (VC-1.2) against a real model.
//!
//! - **VC-1.1 (Happy).** `fork` carries the unchanged prefix `[0..at_tick)`
//!   VERBATIM; replaying that prefix re-fingerprints and REUSES every recorded
//!   model result with ZERO model calls, reconstructing a World byte-identical to
//!   a faithful replay of the original prefix. (Inv 6, 7.)
//! - **VC-1.3 (Edge).** A NO-OP edit (replace an exogenous input with the SAME
//!   value) re-fingerprints identically, so the WHOLE branch is byte-identical to
//!   the original and `replay_branch` reuses the ENTIRE tail — the exploding
//!   client never fires and the branch World is digest-equal to the original
//!   (fork == edit, one rule). (Inv 7.)
//! - **VC-1.4 (Negative).** `fork` REJECTS an edit whose input is a DERIVED
//!   model/tool result, and a `Replace` at a tick whose ORIGINAL slot is derived,
//!   while accepting the exogenous set. (Edit-scope; Codex-r1 #5.)
//!
//! It touches no `src/agent/world/*` file (it consumes the promoted replay spine
//! and the branch materialization layer as a library). See
//! docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), Inv 7
//! (content-addressed replay — the fingerprint), FORK/EDIT algorithm.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use rubberdux::agent::world::branch::{edit_is_exogenous, fork, Edit, EditKind};
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{fingerprint_call, Command, SurfaceDriver};
use rubberdux::provider::{ModelApi, ModelInfo, ModelRequest, ModelResponse};
use std::future::Future;
use std::pin::Pin;

use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::replay::{
    fold_log, genesis_from_log, is_model_call_result, replay_branch, replay_world,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8): inert here (no
// System draws) but it must reseed identically for the folds to match.
const SEED: u64 = 7;

/// The world-default `ModelConfig`. Its `model` id rides in the request the
/// re-emitted `CallModel` fingerprints, so the recorded `ModelResponded` events
/// below are stamped against THIS exact value; no real call is ever made.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-counterfactual-offline".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// The fresh `World` a session starts from: tick 0, a single primary `Idle` root
/// entity, `Resources` reseeded from `seed`. Mirrors the genesis every replay
/// harness builds (the shell owns genesis; no P0 System creates it).
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
            spawned: 0,
            model: None,
            autonomy: None,
        },
    );
    world
}

// ---------------------------------------------------------------------------
// Recorded-log builders — keep the hand-authored session readable
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

fn model_responded(at: u64, cmd: CmdId, text: &str) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            // Stamped to the live request's hash by `stamp_fingerprints` below.
            fingerprint: Fingerprint(String::new()),
            blocks: vec![Block::Text { text: text.into() }],
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-counterfactual-offline".into(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

/// Stamp each model-call result's `fingerprint` with the hash the LIVE driver
/// would record for the request the reducer re-emits for that `cmd`, so a
/// faithful replay REUSES each result instead of diverging (Inv 7). Mirrors the
/// promoted spine's recording discipline (`effects::fingerprint_call`).
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
                let fp = fingerprint_call(messages, tools, params).expect("fingerprint a request");
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

/// A two-turn recorded session with fingerprints stamped so a faithful replay
/// reuses both `ModelResponded` results: turn 1 (`cmd 0`) and turn 2 (`cmd 1`).
fn two_turn_session(model: &ModelConfig) -> Vec<Event> {
    let mut events = vec![
        session_started(),
        user_message(1, "first question"),
        model_responded(2, 0, "first answer"),
        user_message(3, "second question"),
        model_responded(4, 1, "second answer"),
    ];
    stamp_fingerprints(&mut events, model);
    events
}

// ---------------------------------------------------------------------------
// ExplodingClient / NoSurfaceDrive — the structural zero-call witnesses
// ---------------------------------------------------------------------------

/// A `ModelCaller` that PANICS if invoked. Threaded into every offline
/// `replay_branch` here so a single live call (the run crossing the divergence)
/// aborts the test — the structural proof that the prefix / no-op tail reuses
/// with ZERO model calls (Inv 6).
struct ExplodingClient;

impl ModelApi for ExplodingClient {
    fn turn<'a>(
        &'a self,
        _req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        panic!("the offline counterfactual replay must never invoke the model client");
    }
    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn model(&self) -> &str {
        "stub"
    }
}

/// A `SurfaceDriver` that PANICS if reached: these model-only branches never
/// drive a surface, so a reached `drive` would be a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("a CallModel-only branch must not reach the surface driver");
    }
}

// ---------------------------------------------------------------------------
// VC-1.1 — the unchanged prefix reuses byte-identically with zero model calls
// ---------------------------------------------------------------------------

/// VC-1.1: `fork` with a REAL exogenous edit carries the prefix `[0..at_tick)`
/// VERBATIM, and replaying that prefix reuses every recorded model result with
/// ZERO model calls — reconstructing a World BYTE-IDENTICAL to a faithful replay
/// of the original prefix. The [`ExplodingClient`] is structurally unreachable
/// because no Command in the prefix diverges (the edit lives AT `at_tick`, past
/// the prefix), so the run never flips to live. (Inv 6, 7.)
#[tokio::test]
async fn forked_prefix_reuses_byte_identically_with_zero_model_calls() {
    let model = offline_model();
    let original = two_turn_session(&model);

    // Fork at tick 3, REPLACING the turn-2 exogenous input with a DIFFERENT one.
    // The edit lives at tick 3; the prefix `[0..3)` is unchanged.
    let edit = Edit {
        at_tick: 3,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "second question (edited)".into(),
        },
        kind: EditKind::Replace,
    };
    let (_branch_id, branch) = fork(&original, 3, edit).expect("fork an exogenous edit");

    // The branch prefix `[0..3)` is byte-for-byte the original prefix (fork clones
    // it verbatim, derived results carried so it re-fingerprints and reuses).
    let original_prefix: Vec<Event> = original.iter().filter(|e| e.at < 3).cloned().collect();
    let branch_prefix: Vec<Event> = branch.iter().filter(|e| e.at < 3).cloned().collect();
    assert_eq!(
        branch_prefix, original_prefix,
        "VC-1.1: fork must carry the unchanged prefix `[0..at_tick)` byte-for-byte"
    );

    // Replay the branch prefix `[0..3)` (everything BEFORE the edit). It cannot
    // diverge, so the ExplodingClient never fires — ZERO model calls up to the
    // edit. The reconstructed World is byte-identical to a faithful replay of the
    // original prefix.
    let original_prefix_world = replay_world(
        genesis_from_log(&original_prefix, |seed| genesis(seed, &model)).expect("genesis"),
        &original_prefix,
        is_model_call_result,
    )
    .expect("the original prefix replays faithfully");

    let mut log = MemoryEventLog::new();
    let branch_prefix_world = replay_branch(
        genesis_from_log(&branch_prefix, |seed| genesis(seed, &model)).expect("genesis"),
        &branch_prefix,
        is_model_call_result,
        &ExplodingClient,
        &NoSurfaceDrive,
        &mut log,
    )
    .await
    .expect("the unchanged prefix reuses every recorded result with zero live calls");

    assert_eq!(
        serde_json::to_vec(&branch_prefix_world).expect("serialize branch prefix World"),
        serde_json::to_vec(&original_prefix_world).expect("serialize original prefix World"),
        "VC-1.1: the reused prefix reconstructs a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(
        branch_prefix_world, original_prefix_world,
        "VC-1.1: the reused prefix World equals a faithful replay of the original prefix"
    );
    assert!(
        log.load().expect("load the branch prefix log").is_empty(),
        "VC-1.1: zero live results appended — the prefix was reused, not re-run"
    );

    // The original recorded log is untouched by the fork.
    assert_eq!(
        original,
        two_turn_session(&model),
        "VC-1.1: fork must not mutate the original recorded log"
    );
}

// ---------------------------------------------------------------------------
// VC-1.3 — a no-op edit reuses the ENTIRE tail (fork == edit, one rule)
// ---------------------------------------------------------------------------

/// VC-1.3: a NO-OP edit — `Replace` the turn-2 exogenous input with the SAME
/// value — re-fingerprints identically, so the materialized branch is
/// byte-identical to the original and `replay_branch` reuses the ENTIRE tail with
/// ZERO live calls. The branch World is digest-equal to both `fold_log` and
/// `replay_world` of the original; the [`ExplodingClient`] never fires and nothing
/// is appended to the branch log. (Inv 7.)
#[tokio::test]
async fn no_op_edit_reuses_the_entire_tail_with_zero_live_calls() {
    let model = offline_model();
    let original = two_turn_session(&model);

    // The no-op edit replaces the turn-2 user message with the SAME text it
    // already holds, so the branch is identical to the original (fork == edit).
    let edit = Edit {
        at_tick: 3,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "second question".into(),
        },
        kind: EditKind::Replace,
    };
    let (_branch_id, branch) = fork(&original, 3, edit).expect("fork a no-op edit");
    assert_eq!(
        branch, original,
        "VC-1.3: a no-op edit must materialize a branch identical to the original"
    );

    // The canonical faithful reconstructions the unchanged branch must reproduce.
    let folded = fold_log(
        genesis_from_log(&original, |seed| genesis(seed, &model)).expect("genesis"),
        &original,
    );
    let replayed = replay_world(
        genesis_from_log(&original, |seed| genesis(seed, &model)).expect("genesis"),
        &original,
        is_model_call_result,
    )
    .expect("the original log replays faithfully");

    let mut log = MemoryEventLog::new();
    let branch_world = replay_branch(
        genesis_from_log(&branch, |seed| genesis(seed, &model)).expect("genesis"),
        &branch,
        is_model_call_result,
        &ExplodingClient,
        &NoSurfaceDrive,
        &mut log,
    )
    .await
    .expect("a no-op edit reuses the whole tail with zero live calls");

    assert_eq!(
        serde_json::to_vec(&branch_world).expect("serialize branch World"),
        serde_json::to_vec(&folded).expect("serialize folded World"),
        "VC-1.3: a no-op branch is byte-identical to fold_log of the original (Inv 7)"
    );
    assert_eq!(
        branch_world, replayed,
        "VC-1.3: replay_branch over a no-op branch == replay_world over the original"
    );
    assert!(
        log.load().expect("load the branch log").is_empty(),
        "VC-1.3: zero live results appended — the ENTIRE tail was reused"
    );
}

// ---------------------------------------------------------------------------
// VC-1.4 — a derived-input edit is rejected; the exogenous set is accepted
// ---------------------------------------------------------------------------

/// VC-1.4: editing a DERIVED input (a model/tool result) is rejected by `fork`,
/// while exogenous edits are accepted. Three rejection facets and the positive
/// acceptance are checked: (a) the edit INPUT is a derived `ModelResponded`; (b)
/// the edit input is exogenous but the ORIGINAL slot at `at_tick` is derived
/// (the slot-check); (c) the classifier `edit_is_exogenous` partitions the set.
/// (Edit-scope; Codex-r1 #5.)
#[tokio::test]
async fn derived_input_edit_is_rejected_exogenous_accepted() {
    let model = offline_model();
    let original = two_turn_session(&model); // tick 2 / tick 4 are derived ModelResponded

    // (a) The edit INPUT is a derived model result → rejected, even though tick 2
    // is the matching slot.
    let derived_input_edit = Edit {
        at_tick: 2,
        input: model_responded(2, 0, "tampered answer").input,
        kind: EditKind::Replace,
    };
    assert!(
        fork(&original, 2, derived_input_edit).is_err(),
        "VC-1.4: editing a DERIVED model result must be rejected"
    );

    // (b) The edit input is EXOGENOUS, but the ORIGINAL slot at tick 2 is a
    // derived `ModelResponded` → still rejected (a derived slot is never edited in
    // place; rewind and re-run live instead).
    let derived_slot_edit = Edit {
        at_tick: 2,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "try to overwrite the model output".into(),
        },
        kind: EditKind::Replace,
    };
    assert!(
        fork(&original, 2, derived_slot_edit).is_err(),
        "VC-1.4: a Replace at a DERIVED slot must be rejected even with an exogenous input"
    );

    // (c) The exogenous set is accepted: forking at tick 3 (the turn-2 user
    // message) with a new user message succeeds.
    let exogenous_edit = Edit {
        at_tick: 3,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "a different second question".into(),
        },
        kind: EditKind::Replace,
    };
    assert!(
        fork(&original, 3, exogenous_edit).is_ok(),
        "VC-1.4: an exogenous edit at an exogenous slot must be accepted"
    );

    // The classifier the fork gate delegates to partitions the set directly.
    assert!(
        !edit_is_exogenous(&model_responded(0, 0, "x").input),
        "VC-1.4: a ModelResponded is derived, not exogenous"
    );
    assert!(
        edit_is_exogenous(&LogicalInput::UserMessage {
            to: 0,
            text: "free variable".into(),
        }),
        "VC-1.4: a UserMessage is exogenous"
    );
}
