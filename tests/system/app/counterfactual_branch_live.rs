//! **VC-1.2** — the counterfactual replay boundary's LIVE half: a recorded
//! session is forked with an exogenous edit, and `replay_branch` flips
//! Replay→Live **exactly once** at the divergence tick, calling a REAL model past
//! the edit and appending the fresh result to the BRANCH log. (Inv 6, 7.)
//!
//! The offline half (`tests/integration/agent/world_counterfactual_replay.rs`,
//! VC-1.1/1.3/1.4) proves byte-identical prefix reuse, no-op tail reuse, and the
//! derived-edit rejection with an exploding client — i.e. the REPLAY side of the
//! boundary, structurally. This case proves the LIVE side that the exploding
//! client cannot: that the run actually crosses the divergence and pays for a
//! real continuation. Per the mock-data policy (root `CLAUDE.md`) it uses the real
//! model and is gated on live-LLM credentials, skipping cleanly when absent.
//!
//! It needs no macOS surface, no VM, and no subprocess worker: it drives the
//! same promoted replay spine (`replay::replay_branch`) and branch-materialization
//! layer (`branch::fork`) the production runtime uses, in-process, against a real
//! `MessagesClient`. Because record and replay share ONE genesis `ModelConfig`,
//! the recorded `ModelResponded` fingerprints and the re-emitted `CallModel`
//! hashes are self-consistent — so a faithful prefix reuses with ZERO calls and
//! only the edited turn diverges.
//!
//! ## The session and the proof
//! 1. **Record (2 real calls).** Drive two live turns through `replay_branch`
//!    (each turn diverges off the end of the recorded cursor → goes live →
//!    appends a real `ModelResponded`): `q1 → a1`, then `q2 → a2`. The result is a
//!    genuine recorded `[SessionStarted, UM(q1), MR(a1), UM(q2), MR(a2)]` whose
//!    fingerprints a faithful replay reuses.
//! 2. **Fork.** `fork(original, at_tick = turn-2 tick, Replace q2 → q2-EDITED)`
//!    carries the verbatim prefix `[SessionStarted, UM(q1), MR(a1)]` and the
//!    re-stamped edited tail.
//! 3. **Branch-replay (1 real call).** `replay_branch` REUSES turn 1 (its
//!    re-emitted request re-hashes to the recorded `a1` fingerprint → ZERO calls),
//!    then at the FIRST `Diverged` — the edited turn-2 `CallModel`, whose request
//!    no longer matches the recorded `a2` fingerprint — flips Replay→Live, makes
//!    ONE real model call, and appends the fresh `MR` to the BRANCH log. A
//!    counting wrapper asserts EXACTLY ONE branch-phase call; the appended branch
//!    log holds EXACTLY ONE fresh result; and the branch World DIVERGES from a
//!    faithful replay of the original.
//!
//! See `docs/agent/world/ecs-runtime.md` (Inv 6 replay determinism; Inv 7
//! content-addressed replay; One-way replay → live handoff) and the plan's
//! Verification §1 VC-1.2.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::branch::{fork, Edit, EditKind};
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{Command, ModelCaller, SurfaceDriver};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{Event, LogicalInput, ModelMeta, Origin};
use rubberdux::agent::world::model_client::MessagesClient;
use rubberdux::agent::world::replay::{self, is_model_call_result, replay_branch, replay_world};
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

use crate::live_gate::skip_without_live_llm;

// The hidden RNG seed crossing the recorded boundary (Inv 8): the same value
// reseeds genesis on every fold so the recording and the branch replay are
// comparable.
const SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Genesis — the World construction reconstructed identically on every fold
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from, mirroring the worker's
/// `WorldDriver::bootstrap`: tick 0, a single primary `Idle` root entity, and
/// `Resources` seeded from `seed` with the supplied `model`. Record and replay
/// BOTH start here and fold the SAME recorded header, which is what makes the
/// re-emitted `CallModel` re-hash to the recorded `ModelResponded` fingerprint.
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

/// Rebuild genesis from the recorded log: the seed is the one hidden input that
/// must cross a recorded boundary, so every fold reseeds `Rng` from the log's
/// `SessionStarted` header.
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

/// The genesis `ModelConfig` shared by record and replay. `model` is the request
/// alias resolved from `RUBBERDUX_LLM_MODEL` (the value that rides in the
/// `/v1/messages` body, so a REAL call targets a valid model), `max_tokens` is
/// read from `RUBBERDUX_LLM_MAX_TOKENS` (default 1024 — a short one-word answer
/// stops well short of it), and effort is `Medium`. Because both phases share this
/// exact value, the recorded fingerprints and the re-emitted request hashes are
/// self-consistent.
fn shared_model_config(model_alias: &str) -> ModelConfig {
    let max_tokens = std::env::var("RUBBERDUX_LLM_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1024);
    ModelConfig {
        model: model_alias.to_string(),
        max_tokens,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// Recorded-log builders
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

// ---------------------------------------------------------------------------
// CountingCaller — wrap the real client to count the branch-phase model calls
// ---------------------------------------------------------------------------

/// A `ModelCaller` decorator that COUNTS each call and forwards to a real
/// `MessagesClient`. Threading it through the branch replay makes "flip
/// Replay→Live exactly once" a runtime assertion (`calls() == 1`): turn 1 reuses
/// its recorded result with zero calls, and only the edited turn-2 divergence
/// reaches the model.
struct CountingCaller<'a> {
    inner: &'a MessagesClient,
    calls: AtomicUsize,
}

impl<'a> CountingCaller<'a> {
    fn new(inner: &'a MessagesClient) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ModelCaller for CountingCaller<'_> {
    async fn call(&self, request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.call(request_body).await
    }
}

/// A `SurfaceDriver` that PANICS if reached: this CallModel-only session never
/// drives a surface, so a reached `drive` would be a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("a CallModel-only branch must not reach the surface driver");
    }
}

// ---------------------------------------------------------------------------
// record_live_turn — drive one live turn and append its real result
// ---------------------------------------------------------------------------

/// Append a user turn to the recorded log and drive it LIVE, returning the log
/// extended with the turn's real `ModelResponded`.
///
/// Replays the prior recorded log under `replay_branch`: every already-recorded
/// turn REUSES its result by fingerprint (zero calls), and the NEW user message —
/// which has no recorded result yet — diverges off the end of the cursor, flips
/// the run to live, makes ONE real model call, and appends the result. The
/// appended events are concatenated so the next turn folds the full history.
async fn record_live_turn<C: ModelCaller>(
    prior: &[Event],
    user_text: &str,
    model: &ModelConfig,
    client: &C,
) -> Result<Vec<Event>, Error> {
    let next_at = prior.iter().map(|e| e.at).max().map_or(0, |m| m + 1);
    let mut events = prior.to_vec();
    events.push(user_message(next_at, user_text));

    let mut log = MemoryEventLog::new();
    replay_branch(
        genesis_from_log(&events, model)?,
        &events,
        is_model_call_result,
        client,
        &NoSurfaceDrive,
        &mut log,
    )
    .await?;

    events.extend(log.load()?);
    Ok(events)
}

/// Count the `ModelResponded` / `ModelFailed` inputs in a log, for the recording
/// health check (a failed live call records a `ModelFailed`, which would make the
/// fingerprints unusable).
fn count_results(events: &[Event]) -> (usize, usize) {
    let responded = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::ModelResponded { .. }))
        .count();
    let failed = events
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::ModelFailed { .. }))
        .count();
    (responded, failed)
}

// ---------------------------------------------------------------------------
// VC-1.2 — record a real session, fork, flip Replay→Live exactly once
// ---------------------------------------------------------------------------

/// VC-1.2 entry point (dispatched from `tests/system/main.rs`). Records a real
/// two-turn session, forks with an exogenous edit at turn 2, and proves
/// `replay_branch` flips Replay→Live exactly once at the divergence with a fresh
/// real-model result appended to the branch log.
pub async fn run() {
    if skip_without_live_llm("app::counterfactual_branch_live (VC-1.2)") {
        return;
    }

    let client = MessagesClient::from_env()
        .expect("build a MessagesClient from RUBBERDUX_LLM_* for the live branch call");
    let model = shared_model_config(client.model());
    eprintln!(
        "[VC-1.2] live branch fingerprint ModelConfig: model={:?} max_tokens={} effort={:?}",
        model.model, model.max_tokens, model.effort
    );

    // -- (1) Record a real two-turn session (2 real model calls) --------------
    let original = vec![session_started()];
    let original = record_live_turn(
        &original,
        "Reply with exactly one word: the capital of France.",
        &model,
        &client,
    )
    .await
    .expect("record turn 1 live");
    let original = record_live_turn(
        &original,
        "Reply with exactly one word: the capital of Japan.",
        &model,
        &client,
    )
    .await
    .expect("record turn 2 live");

    // The recording must hold two real `ModelResponded` results (no `ModelFailed`)
    // so the fingerprints a faithful replay reuses are real.
    let (responded, failed) = count_results(&original);
    assert!(
        responded == 2 && failed == 0,
        "VC-1.2: the recording must produce two real ModelResponded results (got {responded} \
         responded, {failed} failed). The live call failed — check RUBBERDUX_LLM_* credentials. \
         Recorded inputs: [{}].",
        original
            .iter()
            .map(|e| variant_name(&e.input))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // The turn-2 user message anchors the fork. It is the SECOND `UserMessage` in
    // the recorded log; everything before it is the verbatim prefix to reuse.
    let turn_two_tick = original
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::UserMessage { .. }))
        .nth(1)
        .map(|e| e.at)
        .expect("the recording has a second user message (turn 2)");

    // -- (2) Fork with an exogenous edit at turn 2 ----------------------------
    // Replace the turn-2 question with a DIFFERENT one so its recorded result is
    // STALE (re-fingerprints differently) → the first divergence is exactly here.
    let edit = Edit {
        at_tick: turn_two_tick,
        input: LogicalInput::UserMessage {
            to: 0,
            text: "Reply with exactly one word: the capital of Italy.".into(),
        },
        kind: EditKind::Replace,
    };
    let (branch_id, branch) =
        fork(&original, turn_two_tick, edit).expect("fork an exogenous edit at turn 2");
    eprintln!(
        "[VC-1.2] forked branch {} at tick {turn_two_tick} ({} events)",
        branch_id.0,
        branch.len()
    );

    // The branch prefix `[0..turn_two_tick)` is byte-identical to the original
    // prefix (fork carries it verbatim so it re-fingerprints and reuses).
    let original_prefix: Vec<Event> = original
        .iter()
        .filter(|e| e.at < turn_two_tick)
        .cloned()
        .collect();
    let branch_prefix: Vec<Event> = branch
        .iter()
        .filter(|e| e.at < turn_two_tick)
        .cloned()
        .collect();
    assert_eq!(
        branch_prefix, original_prefix,
        "VC-1.2: the fork prefix must be byte-identical to the original prefix"
    );

    // -- (3) Branch-replay: reuse turn 1, flip to live at turn 2 (1 real call) -
    let counting = CountingCaller::new(&client);
    let mut branch_log = MemoryEventLog::new();
    let branch_world = replay_branch(
        genesis_from_log(&branch, &model).expect("branch genesis"),
        &branch,
        is_model_call_result,
        &counting,
        &NoSurfaceDrive,
        &mut branch_log,
    )
    .await
    .expect("the branch replays the reused prefix then goes live at the edit");

    // The run flipped Replay→Live EXACTLY ONCE: turn 1 reused its recorded result
    // with ZERO calls; only the edited turn-2 divergence reached the model.
    assert_eq!(
        counting.calls(),
        1,
        "VC-1.2: replay_branch must invoke the model EXACTLY ONCE — only past the divergence, \
         never for the reused turn 1 (the one-way Replay→Live flip)"
    );

    // The fresh live result was appended to the BRANCH log (the divergent tail),
    // and only that one result — proving a single flip, not a re-run of the prefix.
    let appended = branch_log.load().expect("load the branch log");
    assert_eq!(
        appended.len(),
        1,
        "VC-1.2: exactly one fresh live result is appended to the branch log (one flip). \
         Appended: [{}].",
        appended
            .iter()
            .map(|e| variant_name(&e.input))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        matches!(appended[0].input, LogicalInput::ModelResponded { .. }),
        "VC-1.2: the appended branch result is a real ModelResponded (a successful live call)"
    );
    // The fresh result is stamped at the next monotonic tick after the whole
    // branch (the live tail follows the recorded branch, Inv 12).
    let branch_max = branch.iter().map(|e| e.at).max().unwrap_or(0);
    assert_eq!(
        appended[0].at,
        branch_max + 1,
        "VC-1.2: the live result is appended at the next tick after the recorded branch"
    );

    // -- The branch World DIVERGES from a faithful replay of the original ------
    let original_world = replay_world(
        genesis_from_log(&original, &model).expect("original genesis"),
        &original,
        is_model_call_result,
    )
    .expect("the unedited original replays faithfully");
    assert_ne!(
        serde_json::to_vec(&branch_world).expect("serialize branch World"),
        serde_json::to_vec(&original_world).expect("serialize original World"),
        "VC-1.2: the branch World must diverge from the original from the edit onward"
    );

    eprintln!(
        "[VC-1.2] PASS: recorded 2 real turns, forked at tick {turn_two_tick}, branch reused \
         turn 1 with 0 calls and flipped Replay→Live EXACTLY ONCE at the divergence (1 real \
         call), appended 1 fresh ModelResponded to the branch log, branch World diverges from \
         the original."
    );
}

/// The `LogicalInput` variant name, for self-explaining diagnostic dumps.
fn variant_name(input: &LogicalInput) -> &'static str {
    match input {
        LogicalInput::SessionStarted { .. } => "SessionStarted",
        LogicalInput::UserMessage { .. } => "UserMessage",
        LogicalInput::ModelResponded { .. } => "ModelResponded",
        LogicalInput::ModelFailed { .. } => "ModelFailed",
        LogicalInput::ToolReturned { .. } => "ToolReturned",
        _ => "<other>",
    }
}
