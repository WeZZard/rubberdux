//! **VC-3.2** — the per-entity model-override LIVE loopback: a sub-agent carrying a
//! `Components.model` override actually issues its `CallModel` against the OVERRIDDEN
//! model, proven with a REAL model call, NO VM, no surface, no subprocess worker.
//!
//! The offline half (`tests/integration/agent/world_entity_overrides.rs`, VC-3.1)
//! proves — deterministically, non-vacuously — that a `Some` model/autonomy override
//! resolves to the entity's value and a `None` inherits the world default, through
//! the real `CallModel`-emit and autonomy-gate paths. This case proves the LIVE side
//! the offline half cannot: that the resolved override model is the one actually put
//! on the wire and answered by a real provider. Per the mock-data policy (root
//! `CLAUDE.md`) it uses the real model and is gated on live-LLM credentials, skipping
//! cleanly when absent.
//!
//! ## The construction and the proof
//! The World's DEFAULT model (`Resources.model`) is a SENTINEL alias the provider
//! would reject — it MUST NOT be the one called. A sub-agent entity (a `Subagent`,
//! lineage = the primary, depth 1) carries `Components.model = Some(real-env-model)`,
//! the only VALID model. Driving the sub-agent's turn through the pure `tick` reducer
//! emits its `CallModel` with `params = World::model_for(child)` — the OVERRIDE. The
//! emitted command is dispatched through the production [`drive_live`] driver against a
//! recording wrapper over the REAL selected provider. Two independent facts pin the
//! override:
//!
//! 1. **The request carries the override.** The recorded `/v1/messages` body's `model`
//!    is the real env model (`MessageBuilder` reads `params.model`), NOT the sentinel
//!    world default.
//! 2. **The call really happened against it.** The driver records a `ModelResponded`
//!    (a successful real call) — which is only possible because the VALID override was
//!    used. Had `model_for` ignored the override and used the sentinel world default,
//!    the call would have FAILED (`ModelFailed`) and this assertion would catch it.
//!    The success against the override IS the proof.
//!
//! The dispatched result Events are persisted under
//! `tests/results/.../system/endurance_overrides_loopback/` for debugging (per
//! `tests/CLAUDE.md`).
//!
//! See `docs/agent/world/ecs-runtime.md` §343-348 (per-entity overrides; `model_for`
//! consulted at every `CallModel` emit) and the plan's Verification §3 VC-3.2.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{
    drive_live, Command, ResultStamp, SurfaceDriver, UnattachedPeerSender,
};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::History;
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};
use rubberdux::agent::world::world::{
    Activity, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;
use rubberdux::provider::{
    ModelApi, ModelInfo, ModelRequest, ModelResponse, selected_from_env,
};
use std::future::Future;
use std::pin::Pin;

use crate::live_gate::skip_without_live_llm;

// The hidden RNG seed crossing the recorded boundary (Inv 8).
const SEED: u64 = 31;

// The primary (lead) and the overriding sub-agent.
const PRIMARY: EntityId = 0;
const SUBAGENT: EntityId = 1;

// The world-default model is a SENTINEL the provider rejects: it stands for "the model
// that MUST NOT be called". Only the sub-agent's override (the real env model) is valid,
// so a successful real call can ONLY have used the override.
const WORLD_DEFAULT_SENTINEL: &str = "endurance-world-default-sentinel-must-not-be-called";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The genesis World: a primary at the SENTINEL world default plus a sub-agent
/// carrying the real-env-model override. Driving the sub-agent's turn resolves
/// `model_for(SUBAGENT)` to the override at the `CallModel` emit.
fn genesis(world_default: &ModelConfig, override_model: &ModelConfig) -> World {
    let mut world = World::new(PRIMARY, Resources::new(SEED, world_default.clone()));
    // The primary/lead, inheriting the (sentinel) world default — never driven here.
    world.entities.insert(
        PRIMARY,
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
    // The sub-agent with a per-entity MODEL override (US-4: "a sub-agent runs a
    // different model than the lead").
    world.entities.insert(
        SUBAGENT,
        Components {
            identity: Identity::Subagent,
            lineage: Lineage {
                parent: Some(PRIMARY),
                depth: 1,
            },
            history: History::default(),
            activity: Activity::Idle,
            gate: EntityGate::default(),
            budget: Budget::default(),
            inbox: Inbox::default(),
            turns: 0,
            spawned: 0,
            model: Some(override_model.clone()),
            autonomy: None,
        },
    );
    world
}

/// The override `ModelConfig` (the real env model the live call must target).
/// `model` is the alias `selected_from_env` resolved from `RUBBERDUX_LLM_MODEL`
/// (so a REAL call hits a valid model); `max_tokens` from `RUBBERDUX_LLM_MAX_TOKENS`
/// (default 1024); effort `Medium`. Mirrors `peer_drive_loopback::shared_model_config`.
fn override_model_config(model_alias: &str) -> ModelConfig {
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

/// The sentinel world-default `ModelConfig` — a model alias the provider rejects, so a
/// successful real call can only have used the sub-agent's override.
fn sentinel_world_default() -> ModelConfig {
    ModelConfig {
        model: WORLD_DEFAULT_SENTINEL.to_string(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// A `SurfaceDriver` that PANICS if reached: this `CallModel`-only loopback never
/// drives a surface, so a reached `drive` would be a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("the override loopback drives only a CallModel; the surface driver must not be reached");
    }
}

/// A [`ModelApi`] that RECORDS the model alias on each neutral request it is handed
/// and then forwards to the REAL config-selected provider. The captured
/// `sampling.model` is the proof the sub-agent's call carried its OVERRIDE — the
/// neutral request is the dialect-independent pivot the wire body is built from, so
/// its `model` is exactly what the provider sends. The guard is dropped before the
/// await so the future stays `Send`.
struct RecordingCaller {
    inner: Box<dyn ModelApi>,
    last_model: Mutex<Option<String>>,
}

impl RecordingCaller {
    fn new(inner: Box<dyn ModelApi>) -> Self {
        Self {
            inner,
            last_model: Mutex::new(None),
        }
    }

    /// The `model` of the last request sent, if any.
    fn last_request_model(&self) -> Option<String> {
        self.last_model
            .lock()
            .expect("lock the recorded request model")
            .clone()
    }
}

impl ModelApi for RecordingCaller {
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        {
            let mut guard = self.last_model.lock().expect("lock the recorded request model");
            *guard = Some(req.sampling.model.clone());
        }
        self.inner.turn(req)
    }
    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        self.inner.list_models()
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
}

// ---------------------------------------------------------------------------
// VC-3.2 entry point (dispatched from `tests/system/main.rs`)
// ---------------------------------------------------------------------------

/// VC-3.2 — drive a sub-agent carrying a model override and prove its real `CallModel`
/// targets the OVERRIDE model, not the (sentinel) world default.
pub async fn run() {
    if skip_without_live_llm("app::endurance_overrides_loopback (VC-3.2)") {
        return;
    }

    let client =
        selected_from_env().expect("build a provider from RUBBERDUX_LLM_* for the live override turn");
    let override_model = override_model_config(client.model());
    let world_default = sentinel_world_default();
    // Non-vacuity precondition: the override is OBSERVABLY distinct from the (sentinel)
    // world default, so "the call used the override" cannot be satisfied by the default.
    assert_ne!(
        override_model.model, world_default.model,
        "the sub-agent override must differ from the sentinel world default for a non-vacuous proof"
    );
    eprintln!(
        "[VC-3.2] override ModelConfig: model={:?} max_tokens={} effort={:?}; sentinel world default={:?}",
        override_model.model, override_model.max_tokens, override_model.effort, world_default.model
    );

    // -- Build the World and drive the sub-agent's turn through the pure reducer ------
    let world = genesis(&world_default, &override_model);
    let initiate = Event {
        origin: Origin::Human,
        edge: 0,
        at: 1,
        wall: None,
        input: LogicalInput::UserMessage {
            to: SUBAGENT,
            text: "Reply with exactly one word: ACK.".into(),
        },
    };
    let (after_intake, commands) =
        rubberdux::agent::world::systems::tick(&world, &initiate);

    // The emitted CallModel routes to the sub-agent and carries the OVERRIDE params —
    // the structural pre-check before the real dispatch.
    let (emitted_params, emitted_entity) = commands
        .iter()
        .find_map(|c| match c {
            Command::CallModel { params, entity, .. } => Some((params.clone(), *entity)),
            _ => None,
        })
        .expect("the sub-agent's turn must emit a CallModel");
    assert_eq!(
        emitted_entity, SUBAGENT,
        "the CallModel routes to the sub-agent entity"
    );
    assert_eq!(
        emitted_params, override_model,
        "VC-3.2: model_for(child) resolved the OVERRIDE at the CallModel emit (not the world default)"
    );

    // -- Dispatch the emitted CallModel LIVE through the production driver -------------
    let recorder = RecordingCaller::new(client);
    let stamp = ResultStamp {
        edge: 0,
        app_edge: 1,
        at: 2,
        wall: None,
    };
    let mut log = MemoryEventLog::new();
    let results = drive_live(
        &commands,
        stamp,
        &after_intake,
        &recorder,
        &NoSurfaceDrive,
        &UnattachedPeerSender,
        &mut log,
    )
    .await
    .expect("the live driver dispatches the sub-agent's override CallModel");

    // -- (1) The request put on the wire carried the OVERRIDE model -------------------
    let request_model = recorder
        .last_request_model()
        .expect("the live driver assembled and sent a /v1/messages request body");
    eprintln!("[VC-3.2] recorded request body model: {request_model:?}");
    assert_eq!(
        request_model, override_model.model,
        "VC-3.2: the sub-agent's request carried its OVERRIDE model on the wire"
    );
    assert_ne!(
        request_model, world_default.model,
        "VC-3.2: the request did NOT carry the sentinel world default"
    );

    // -- (2) A REAL ModelResponded was recorded against the override ------------------
    // Had model_for ignored the override and used the sentinel world default, the call
    // would have FAILED (ModelFailed) — so a recorded ModelResponded against the valid
    // override IS the proof the override was actually used.
    let model_responded = results.iter().find_map(|e| match &e.input {
        LogicalInput::ModelResponded { entity, .. } => Some(*entity),
        _ => None,
    });
    assert!(
        !results
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelFailed { .. })),
        "VC-3.2: the override model is valid and was actually used — no ModelFailed (results: {})",
        variant_dump(&results)
    );
    let responded_entity = model_responded.unwrap_or_else(|| {
        panic!(
            "VC-3.2: the override CallModel must produce a REAL ModelResponded (results: {})",
            variant_dump(&results)
        )
    });
    assert_eq!(
        responded_entity, SUBAGENT,
        "the recorded ModelResponded routes back to the sub-agent (Inv 17)"
    );

    // -- Persist the collected transcript for debugging (tests/CLAUDE.md) -------------
    let recorded = log.load().expect("load the dispatched result log");
    write_transcript(&recorded);

    eprintln!(
        "[VC-3.2] PASS: the sub-agent carrying a model override issued its CallModel against the \
         OVERRIDE model {:?} (request body model={request_model:?}, NOT the sentinel world default \
         {:?}), and a REAL ModelResponded was recorded against it.",
        override_model.model, world_default.model
    );
}

/// A compact `variant: count` dump of a result log, for self-explaining failure output.
fn variant_dump(events: &[Event]) -> String {
    events
        .iter()
        .map(|e| match &e.input {
            LogicalInput::ModelResponded { .. } => "ModelResponded",
            LogicalInput::ModelFailed { .. } => "ModelFailed",
            LogicalInput::UserMessage { .. } => "UserMessage",
            _ => "<other>",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The per-run results directory for the collected transcript, under
/// `tests/results/<unix-millis>/system/endurance_overrides_loopback/` rooted at the
/// crate. A timestamped subdir keeps successive live runs from clobbering one another.
fn results_dir() -> std::path::PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("results")
        .join(format!("{millis}"))
        .join("system")
        .join("endurance_overrides_loopback")
}

/// Write the dispatched result log as raw JSONL plus a short markdown narration, so a
/// failing live run can be inspected (tests/CLAUDE.md).
fn write_transcript(events: &[Event]) {
    let dir = results_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut md = String::from("# Endurance override loopback — dispatched result transcript\n\n");
    for e in events {
        let variant = match &e.input {
            LogicalInput::ModelResponded { entity, .. } => format!("ModelResponded(entity={entity})"),
            LogicalInput::ModelFailed { entity, .. } => format!("ModelFailed(entity={entity})"),
            other => format!("{other:?}"),
        };
        md.push_str(&format!(
            "- at={} edge={} origin={:?} — {variant}\n",
            e.at, e.edge, e.origin
        ));
    }
    let _ = std::fs::write(dir.join("narration.md"), md);
    if let Ok(serialized) = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
    {
        let _ = std::fs::write(dir.join("transcript.jsonl"), serialized.join("\n"));
    }
}
