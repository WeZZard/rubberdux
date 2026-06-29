//! **VC-3.2** — the cross-World peer-drive LIVE loopback: two local Worlds over the
//! REAL [`PeerBroker`], NO VM, the target processing the driven prompt with a REAL
//! model call.
//!
//! The offline half (`tests/integration/agent/world_peer_drive.rs`, VC-3.1/VC-3.3)
//! proves byte-identical replay of a recorded peer-drive branch and the invalid-auth
//! rejection, structurally, with an exploding client. This case proves the LIVE side
//! the exploding client cannot: that a drive emitted by a LEAD reaches a separate
//! TARGET World through the real broker, is recorded there as `DriveRequested` with
//! `Origin::Peer` on its bound peer edge, applies its `surface_ops` to the surface
//! view as a projection, and that the target then processes the driven prompt with a
//! genuine model call. Per the mock-data policy (root `CLAUDE.md`) it uses the real
//! model and is gated on live-LLM credentials, skipping cleanly when absent.
//!
//! It needs no macOS surface, no VM, and no subprocess worker. The two ends are:
//!
//! - **LEAD (outbound seam).** The lead's `SendPeer{Drive}` is driven through the
//!   real [`BrokerPeerSender`] — the exact `PeerSender` the production `drive_live`
//!   `SendPeer` arm invokes — relayed by the shared [`PeerBroker`]
//!   (`PeerId::is_local` routes the delivery locally). This drives the genuine
//!   broker path deterministically, without a non-deterministic model-driven
//!   `drive_peer` tool call (which the effects/host unit tests already cover).
//! - **TARGET (a real World).** A live [`WorldDriver`] over the real selected provider.
//!   The delivered [`DurableEnvelope`] is bridged to a `DriveRequested` (exactly as
//!   the production worker's `bridge_peer_deliver` does) and submitted; the driver
//!   stamps it `Origin::Peer`, binds + logs the peer edge, projects the surface ops,
//!   and enqueues the prompt. A benign initiation then drains the parked prompt and
//!   the target answers with a REAL model call.
//!
//! The target's `world-events.jsonl` is the collected transcript (persisted under
//! `tests/results/.../system/peer_drive_loopback/` for debugging, per `tests/CLAUDE.md`).
//!
//! See `docs/agent/world/ecs-runtime.md` (PeerDriveSystem; DriveRequested; the
//! World↔broker boundary; Theme 4a/4b) and the plan's Verification §3 VC-3.2.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::driver::WorldDriver;
use rubberdux::agent::world::effects::{Command, CommandKey, PeerSender, SurfaceDriver};
use rubberdux::agent::world::event_log::{EventLog, FilesystemEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Role};
use rubberdux::agent::world::inputs::{
    DriveCommand, DurableEnvelope, Event, LogicalInput, Origin, PeerPayload,
};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::surface::{PeerEnvelopeId, SurfaceOp};
use rubberdux::provider::selected_from_env;
use rubberdux::agent::world::world::{
    Activity, Components, Effort, Identity, Inbox, Lineage, ModelConfig, PeerId as WorldPeerId,
    Resources, World,
};
use rubberdux::app::peer::PeerId as AppPeerId;
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::AppId;
use rubberdux::error::Error;
use rubberdux::host::{BrokerPeerSender, PeerBroker};
use rubberdux::protocol::HostToAgent;

use crate::live_gate::skip_without_live_llm;

// The hidden RNG seed crossing the recorded boundary (Inv 8): the target's genesis
// reseeds from its `SessionStarted` header so folding the recorded log reconstructs
// the same World the live driver built.
const SEED: u64 = 31;

// The surface the drive targets; bumped 0→1 by the projected `SetValue`.
const DRIVEN_SURFACE: u32 = 9;

// The App ids of the two local Worlds on the broker.
const LEAD_APP: &str = "peer-drive-lead";
const TARGET_APP: &str = "peer-drive-target";

// ---------------------------------------------------------------------------
// Genesis + helpers (mirroring the worker's WorldDriver::bootstrap genesis)
// ---------------------------------------------------------------------------

/// The fresh `World` the target session starts from, mirroring the worker's
/// bootstrap: tick 0, a single primary `Idle` root entity, `Resources` seeded from
/// `seed`. Folding the recorded log from this genesis reconstructs the live World.
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

/// The genesis `ModelConfig` shared by the live driver and the post-hoc fold. `model`
/// is the request alias resolved from `RUBBERDUX_LLM_MODEL` (so a REAL call targets a
/// valid model); `max_tokens` from `RUBBERDUX_LLM_MAX_TOKENS` (default 1024); effort
/// `Medium`. Mirrors `counterfactual_branch_live::shared_model_config`.
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

/// A `SurfaceDriver` that PANICS if reached: the target is bootstrapped with no
/// surface tools, so its model is never offered `set_value` and the live driver
/// never dispatches a surface `RunTool`. A reached `drive` would be a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("the peer-drive loopback target is offered no surface tools; the surface driver must not be reached");
    }
}

// ---------------------------------------------------------------------------
// VC-3.2 entry point (dispatched from `tests/system/main.rs`)
// ---------------------------------------------------------------------------

/// VC-3.2 — boot two local Worlds over the real broker, drive the target from the
/// lead, and prove the target records `DriveRequested` with `Origin::Peer`, projects
/// the surface ops, and processes the prompt with a REAL model call.
pub async fn run() {
    if skip_without_live_llm("app::peer_drive_loopback (VC-3.2)") {
        return;
    }

    let client = selected_from_env()
        .expect("build a provider from RUBBERDUX_LLM_* for the live target turn");
    let model = shared_model_config(client.model());
    eprintln!(
        "[VC-3.2] live loopback ModelConfig: model={:?} max_tokens={} effort={:?}",
        model.model, model.max_tokens, model.effort
    );

    // -- The shared broker (a switch) the two local Worlds route through ----------
    let home = tempfile::tempdir().expect("tempdir for the broker store");
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(home.path().join("apps")));
    let broker = Arc::new(PeerBroker::new(store));

    let lead_peer = AppPeerId::local(AppId(LEAD_APP.into()));
    let target_peer = AppPeerId::local(AppId(TARGET_APP.into()));

    // Register both ends in the directory; the target additionally gets a live
    // delivery sink so the broker resolves a `Delivered` (rather than queueing to an
    // inbox) — the loopback delivery the worker bridge would consume.
    let (lead_tx, _lead_rx) = tokio::sync::mpsc::channel::<HostToAgent>(8);
    let (target_tx, mut target_rx) = tokio::sync::mpsc::channel::<HostToAgent>(16);
    broker.register(lead_peer.clone(), lead_tx).await;
    broker.register(target_peer.clone(), target_tx).await;

    // -- The TARGET: a real live World over the real selected provider ------------
    // Its event log lives under the results dir so the recorded transcript is
    // collected for debugging (tests/CLAUDE.md).
    let artifact_dir = results_dir();
    std::fs::create_dir_all(&artifact_dir).expect("create the loopback results dir");
    let world_events = artifact_dir.join("world-events.jsonl");
    let mut target = WorldDriver::bootstrap(
        SEED,
        model.clone(),
        client,
        NoSurfaceDrive,
        FilesystemEventLog::new(world_events.clone()),
    )
    .expect("bootstrap the target World driver");

    // -- The LEAD emits SendPeer{Drive} through the REAL broker -------------------
    let lead = BrokerPeerSender::new(broker.clone(), lead_peer.clone());
    let drive = DriveCommand {
        surface_ops: vec![SurfaceOp::SetValue {
            surface: DRIVEN_SURFACE,
            element: 2,
            value: serde_json::json!("driven by peer"),
            base_version: None,
        }],
        prompt: Some("Reply with exactly one word: ACK.".into()),
    };
    let to_world = WorldPeerId {
        app_id: TARGET_APP.into(),
        node_id: "local".into(),
    };
    let outcome = lead
        .send(
            &to_world,
            &PeerPayload::Drive(drive.clone()),
            &CommandKey,
            &PeerEnvelopeId("env-loopback-1".into()),
        )
        .await
        .expect("the lead's SendPeer{Drive} relays through the broker");
    eprintln!("[VC-3.2] lead SendPeer{{Drive}} → broker outcome: {outcome:?}");
    assert_eq!(
        format!("{outcome:?}"),
        "Delivered",
        "the drive must be DELIVERED to the live target sink (local loopback, not queued)"
    );

    // -- The broker delivered a PeerDeliver frame to the target's sink ------------
    let frame = tokio::time::timeout(Duration::from_secs(10), target_rx.recv())
        .await
        .expect("the broker delivers the PeerDeliver frame within the deadline")
        .expect("the target sink yields the delivered frame");
    let (from, payload) = match frame {
        HostToAgent::PeerDeliver { from, payload } => (from, payload),
        other => panic!("expected a PeerDeliver frame, got {other:?}"),
    };
    assert_eq!(from, LEAD_APP, "the delivered frame names the lead App as sender");

    // -- Bridge the durable envelope to a DriveRequested (as the worker does) -----
    let durable: DurableEnvelope =
        serde_json::from_value(payload).expect("the delivered payload is a DurableEnvelope");
    let drive_cmd = match durable.payload {
        PeerPayload::Drive(d) => d,
        PeerPayload::Message(_) => panic!("the loopback delivered a Drive, not a Message"),
    };
    let drive_requested = LogicalInput::DriveRequested {
        from: WorldPeerId {
            app_id: from.clone(),
            node_id: "local".into(),
        },
        envelope: durable.id,
        drive: drive_cmd,
        auth: durable.auth,
    };

    // -- The target folds the drive: Origin::Peer, peer edge, surface projection,
    //    prompt enqueued (no model call yet) ------------------------------------
    let after_drive = target
        .submit(drive_requested)
        .await
        .expect("the target folds the inbound DriveRequested");
    eprintln!(
        "[VC-3.2] target folded DriveRequested ({} entry notifications)",
        after_drive.len()
    );

    // -- Initiate: drain the parked prompt and process it with a REAL model call --
    let answered = target
        .submit(LogicalInput::UserMessage {
            to: 0,
            text: "Proceed with the pending request.".into(),
        })
        .await
        .expect("the target processes the driven prompt with a real model call");
    assert!(
        !answered.is_empty(),
        "VC-3.2: the target's real model turn must stream at least one entry notification"
    );

    // -- Inspect the collected transcript (the target's recorded World log) -------
    let recorded = FilesystemEventLog::new(world_events.clone())
        .load()
        .expect("load the target's recorded World log");
    write_narration(&artifact_dir, &recorded);

    // The drive was recorded as `DriveRequested` with `Origin::Peer`, on a bound peer
    // edge (NOT the human edge 0), and an `EdgeBound` for that edge precedes it.
    let drive_event = recorded
        .iter()
        .find(|e| matches!(e.input, LogicalInput::DriveRequested { .. }))
        .expect("the recorded log holds the inbound DriveRequested");
    assert_eq!(
        drive_event.origin,
        Origin::Peer,
        "the inbound drive is recorded with Origin::Peer"
    );
    assert_ne!(
        drive_event.edge, 0,
        "the inbound drive lands on its bound PEER edge, not the human edge (0)"
    );
    let has_edge_bound = recorded.iter().any(|e| {
        matches!(&e.input, LogicalInput::EdgeBound { edge, counterpart }
            if *edge == drive_event.edge
                && matches!(counterpart, rubberdux::agent::world::world::Counterpart::Peer(_)))
    });
    assert!(
        has_edge_bound,
        "the peer edge was minted and logged as EdgeBound{{Peer}} before the drive (Inv 6)"
    );

    // A REAL model call was recorded (the driven prompt was actually processed).
    let model_calls = recorded
        .iter()
        .filter(|e| matches!(e.input, LogicalInput::ModelResponded { .. }))
        .count();
    assert!(
        model_calls >= 1,
        "VC-3.2: the target must have made at least one REAL model call (got {model_calls})"
    );

    // -- Fold the recorded log to inspect the settled World -----------------------
    let folded = replay::fold_log(
        replay::genesis_from_log(&recorded, |seed| genesis(seed, &model))
            .expect("reseed genesis from the recorded header"),
        &recorded,
    );

    // The drive's surface op applied as a PROJECTION of the DriveRequested input (no
    // separate SurfaceMutated): the driven surface bumped 0→1.
    assert_eq!(
        folded.resources.surfaces.get(&DRIVEN_SURFACE).map(|s| s.version),
        Some(1),
        "VC-3.2: the drive's surface_ops applied to the target's surface view (0→1)"
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|e| matches!(e.input, LogicalInput::SurfaceMutated { .. }))
            .count(),
        0,
        "the drive is the SOLE surface record — no separate SurfaceMutated (Inv 18)"
    );

    // The driven prompt was processed: drained from the Inbox into History FIRST (the
    // root is now Idle with at least the prompt + a real assistant answer).
    let entity = folded.entities.get(&0).expect("target root entity");
    assert!(
        entity.inbox.pending.is_empty(),
        "the parked prompt was drained from the Inbox on initiation"
    );
    let prompt_processed = entity.history.0.iter().any(|m| {
        matches!(m.role, Role::User)
            && m.content
                .iter()
                .any(|b| matches!(b, Block::Text { text } if text.contains("ACK")))
    });
    assert!(
        prompt_processed,
        "VC-3.2: the driven prompt was processed (it landed in the target's History)"
    );
    let has_assistant_answer = entity
        .history
        .0
        .iter()
        .any(|m| matches!(m.role, Role::Assistant));
    assert!(
        has_assistant_answer,
        "VC-3.2: the target produced a real assistant answer to the driven prompt"
    );
    assert!(
        matches!(entity.activity, Activity::Idle),
        "the target turn settled to Idle"
    );

    eprintln!(
        "[VC-3.2] PASS: lead drove the target over the real broker (Delivered); target recorded \
         DriveRequested(Origin::Peer) on peer edge {}, projected the surface op (S{DRIVEN_SURFACE} \
         0→1), and processed the driven prompt with {model_calls} real model call(s). Transcript: {}",
        drive_event.edge,
        world_events.display()
    );
}

/// The per-run results directory for the collected transcript, under
/// `tests/results/<unix-millis>/system/peer_drive_loopback/` rooted at the crate.
/// A timestamped subdir keeps successive live runs from clobbering one another.
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
        .join("peer_drive_loopback")
}

/// Write a human-readable narration of the target's recorded World log alongside the
/// raw JSONL transcript, so a failing live run can be inspected (tests/CLAUDE.md).
fn write_narration(dir: &std::path::Path, events: &[Event]) {
    let mut md = String::from("# Peer-drive loopback — target World transcript\n\n");
    for e in events {
        let variant = match &e.input {
            LogicalInput::SessionStarted { .. } => "SessionStarted".to_string(),
            LogicalInput::EdgeBound { edge, .. } => format!("EdgeBound(edge={edge})"),
            LogicalInput::DriveRequested { from, .. } => format!("DriveRequested(from={from:?})"),
            LogicalInput::UserMessage { text, .. } => format!("UserMessage({text:?})"),
            LogicalInput::ModelResponded { .. } => "ModelResponded".to_string(),
            other => format!("{other:?}"),
        };
        md.push_str(&format!(
            "- at={} edge={} origin={:?} — {variant}\n",
            e.at, e.edge, e.origin
        ));
    }
    let _ = std::fs::write(dir.join("narration.md"), md);
    // Mirror the raw JSONL next to the narration as a convenience copy.
    if let Ok(serialized) = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
    {
        let _ = std::fs::write(dir.join("transcript.jsonl"), serialized.join("\n"));
    }
}
