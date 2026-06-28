//! Hand-authored cross-World peer-drive sessions that fold and replay
//! BYTE-IDENTICALLY with zero model calls (an exploding client), plus the
//! invalid-authorization rejection invariant — the OFFLINE, deterministic half of
//! the PA-sink verification (VC-3.1 + VC-3.3).
//!
//! It composes the same `src/agent/world/` replay spine the surface-mode and
//! session-tools replay sinks use (`replay::fold_log` as the canonical "live"
//! World, `replay::replay_world` as the cursor-driven REPLAY), but over logs that
//! exercise the peer-drive data path:
//!
//! - **OUTBOUND (VC-3.1, sender perspective)** — a `drive_peer` tool turn opens a
//!   `Peer` slot, PeerDriveSystem emits `Command::SendPeer`, and the recorded
//!   `PeerSendOutcome` settles the slot. `drive_replay` fingerprints the re-emitted
//!   `SendPeer` (`fingerprint_peer`) and REUSES the recorded `PeerSendOutcome` from
//!   the cursor (the SOLE dual, Theme 4b) — so the branch replays with ZERO live
//!   sends and ZERO model calls. A wrong settle would re-fingerprint differently and
//!   `Diverged`, so the byte-identity is NON-VACUOUS.
//! - **INBOUND (VC-3.1, target perspective)** — a `DriveRequested` (Origin::Peer,
//!   valid auth) applies its `surface_ops` as a PROJECTION of that input (no separate
//!   `SurfaceMutated`, Inv 18) and enqueues its `prompt` to the Inbox; a following
//!   initiation drains the parked prompt and the target processes it. The folded and
//!   replayed Worlds are byte-identical.
//! - **REJECTION (VC-3.3)** — a `DriveRequested` carrying an absent OR mismatched
//!   `Authorization` is REJECTED by the inbound transition: no surface mutation, no
//!   prompt enqueued. The World after the invalid drive equals the World folded
//!   WITHOUT it (the drive is a logged no-op), and it still replays byte-identically.
//!
//! It touches no `src/agent/world/*` file and makes no live model call, so it needs
//! no credentials and runs on every developer machine.
//!
//! See docs/agent/world/ecs-runtime.md — Inv 6 (replay determinism), Inv 7
//! (content-addressed replay — the fingerprint), Inv 18 (the agent UI-write echo is
//! one fact), PeerDriveSystem; DriveRequested; PeerSendOutcome.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value as Json;

use rubberdux::agent::world::effects::{
    Command, ModelCaller, ToolSet, fingerprint_call, fingerprint_peer,
};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::inputs::{
    Authorization, Capabilities, DeliveryOutcome, DriveCommand, Event, Fingerprint, LogicalInput,
    ModelMeta, Origin, PeerPayload, ReasoningPolicy, StopReason, Usage,
};
use rubberdux::agent::world::replay;
use rubberdux::agent::world::surface::{PeerEnvelopeId, SurfaceOp};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Counterpart, Effort, Identity, Lineage, ModelConfig, PeerId,
    Resources, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8). Inert here (no System
// draws), but it must reseed identically for the two folds to match.
const SEED: u64 = 7;

// The outbound peer-drive tool the sender's model calls; the value the authored
// `SessionStarted` records into `surface_tools` so the re-emitted root `CallModel`
// is offered it and its `Fingerprint` matches the recorded `ModelResponded`.
const DRIVE_PEER: &str = "drive_peer";

// The peer edge an inbound `DriveRequested` binds to (the human edge is 0). Inert to
// the surface projection / prompt enqueue under test (apply_drive ignores the edge),
// but authored to mirror the driver's real log shape.
const PEER_EDGE: u32 = 1;

// The surface the drives target. Bumped 0→1 by a single projected `SetValue`.
const DRIVEN_SURFACE: u32 = 9;

// ---------------------------------------------------------------------------
// Genesis — empty surface tools; the SessionStarted fold reconstructs them
// ---------------------------------------------------------------------------

/// The fresh `World` a session starts from: tick 0, a single primary `Idle` root
/// entity, `Resources` seeded from `seed` with EMPTY `surface_tools`. The recorded
/// `SessionStarted` fold — not genesis — reconstructs `surface_tools`, so BOTH the
/// live fold and the replay start here and fold the SAME header. Mirrors the other
/// replay harnesses' genesis.
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
            budget: rubberdux::agent::world::budget::Budget::default(),
            inbox: rubberdux::agent::world::world::Inbox::default(),
            turns: 0,
            spawned: 0,
            model: None,
        },
    );
    world
}

/// Rebuild genesis from the recorded log: the seed is the one hidden input that must
/// cross a recorded boundary, so replay reseeds `Rng` from the log's `SessionStarted`
/// header. `surface_tools` is reconstructed by FOLDING that same header.
fn genesis_from_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::genesis_from_log(events, |seed| genesis(seed, model))
}

/// The world-default `ModelConfig`. Its `model` id rides in the request the
/// re-emitted `CallModel` fingerprints, so a recorded `ModelResponded` is stamped
/// against THIS exact value; it makes no real call.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-peer-drive-replay".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

// ---------------------------------------------------------------------------
// ExplodingClient — the runtime witness that replay never reaches a model client
// ---------------------------------------------------------------------------

/// A `ModelCaller` that records any invocation and then panics. `drive_replay` takes
/// NO client, so this can never be threaded into `replay_log` by construction;
/// constructing it and asserting its counter stays zero makes the "zero model calls"
/// guarantee (Inv 6) explicit, while the panic is the structural backstop.
struct ExplodingClient {
    calls: Arc<AtomicUsize>,
}

impl ModelCaller for ExplodingClient {
    async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("the replay driver must never invoke the model client");
    }
}

// ---------------------------------------------------------------------------
// fold_log / replay_log — the LIVE canonical World vs. the cursor-driven replay
// ---------------------------------------------------------------------------

/// Fold the WHOLE recorded log through the pure `tick` reducer, event by event.
/// Every input — the exogenous free variables and the already-recorded DERIVED
/// results (`ModelResponded`, `PeerSendOutcome`) — is present, so emitted Commands
/// are discarded. This is the canonical ("live") World the replay must reproduce.
fn fold_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    Ok(replay::fold_log(genesis_from_log(events, model)?, events))
}

/// Fold the SAME recorded log from genesis under the promoted REPLAY driver
/// (`replay::replay_world`): re-apply the exogenous events directly while the
/// `ReplayCursor` stands in for every DERIVED result (`!replay::is_exogenous`) — a
/// `ModelResponded` for a re-emitted `CallModel`, a `PeerSendOutcome` for a
/// re-emitted `SendPeer` — gated on the re-emitted request re-hashing to its
/// `Fingerprint`. A `Diverged` outcome is reported as an error: a faithful replay
/// must reuse every result.
fn replay_log(events: &[Event], model: &ModelConfig) -> Result<World, Error> {
    replay::replay_world(genesis_from_log(events, model)?, events, |input| {
        !replay::is_exogenous(input)
    })
}

/// Stamp every recorded DERIVED result's `fingerprint` with the hash the LIVE driver
/// records for the request the reducer re-emits for that `cmd`: a `ModelResponded` is
/// stamped over `fingerprint_call(messages, tools, params)` and a `PeerSendOutcome`
/// over `fingerprint_peer(to, payload)`. Folding the log live captures each emitted
/// `CallModel`/`SendPeer` by `cmd` (the fingerprint is not an input to id minting, so
/// the cmds are the same whether or not the recorded results are stamped yet), so the
/// replay REUSES each result instead of diverging (Inv 7). Mirrors the replay spine's
/// own `stamp_fingerprints`, extended for the peer-send dual.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig) {
    let mut world = genesis_from_log(events, model).expect("genesis for the fingerprint pass");
    let mut by_cmd: BTreeMap<CmdId, Fingerprint> = BTreeMap::new();
    for ev in events.iter() {
        let (next, commands) = tick(&world, ev);
        world = next;
        for command in &commands {
            match command {
                Command::CallModel {
                    cmd,
                    messages,
                    tools,
                    params,
                    ..
                } => {
                    by_cmd.insert(*cmd, fingerprint_call(messages, tools, params).expect("fp call"));
                }
                Command::SendPeer {
                    cmd, to, payload, ..
                } => {
                    by_cmd.insert(*cmd, fingerprint_peer(to, payload).expect("fp peer"));
                }
                _ => {}
            }
        }
    }
    for ev in events.iter_mut() {
        match &mut ev.input {
            LogicalInput::ModelResponded {
                cmd, fingerprint, ..
            }
            | LogicalInput::PeerSendOutcome {
                cmd, fingerprint, ..
            } => {
                if let Some(fp) = by_cmd.get(cmd) {
                    *fingerprint = fp.clone();
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Event builders
// ---------------------------------------------------------------------------

fn session_started(surface_tools: &[&str]) -> Event {
    Event {
        origin: Origin::System,
        edge: 0,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed: SEED,
            surface_tools: surface_tools.iter().map(|s| s.to_string()).collect(),
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

/// A peer id on the local node.
fn peer(app: &str) -> PeerId {
    PeerId {
        app_id: app.into(),
        node_id: "local".into(),
    }
}

/// A recorded `ModelResponded` carrying `blocks` and `stop_reason`, with an EMPTY
/// fingerprint the `stamp_fingerprints` pass fills in. `cmd` correlates it to the
/// `CallModel` the reducer re-emits.
fn model_responded(at: u64, cmd: CmdId, blocks: Vec<Block>, stop: StopReason) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            blocks,
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-peer-drive-replay".into(),
                stop_reason: stop,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

/// An assistant `drive_peer` tool-use block addressing `to` with a `Drive` payload
/// (one `SetValue` op plus an optional prompt) — the OUTBOUND request shape
/// PeerDriveSystem decodes into a `Command::SendPeer`.
fn drive_peer_block(tool_use_id: &str, to: &PeerId, prompt: Option<&str>) -> Block {
    let mut payload = serde_json::json!({
        "kind": "drive",
        "surface_ops": [
            { "op": "set_value", "surface": DRIVEN_SURFACE, "element": 2, "value": "driven by peer", "base_version": null }
        ],
        "prompt": prompt,
    });
    // Keep the JSON minimal/explicit; `prompt: null` is a valid Option<String>.
    if prompt.is_none() {
        payload["prompt"] = Json::Null;
    }
    Block::ToolUse {
        id: tool_use_id.into(),
        name: DRIVE_PEER.into(),
        input: serde_json::json!({
            "to": { "app_id": to.app_id, "node_id": to.node_id },
            "payload": payload,
        }),
    }
}

/// A recorded `PeerSendOutcome` (the sender-local dual of `SendPeer`) settling the
/// `Peer` slot whose `cmd` matches, with an EMPTY fingerprint the stamp pass fills.
fn peer_send_outcome(at: u64, cmd: CmdId, to: &PeerId, outcome: DeliveryOutcome) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::PeerSendOutcome {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            to: to.clone(),
            outcome,
        },
    }
}

/// An `EdgeBound` binding the peer edge to `Counterpart::Peer(from)` (System-origin,
/// EXOGENOUS), mirroring what the driver logs before the FIRST inbound peer input.
fn edge_bound_peer(at: u64, from: &PeerId) -> Event {
    Event {
        origin: Origin::System,
        edge: PEER_EDGE,
        at,
        wall: None,
        input: LogicalInput::EdgeBound {
            edge: PEER_EDGE,
            counterpart: Counterpart::Peer(from.clone()),
        },
    }
}

/// An inbound `DriveRequested` (Origin::Peer) from `from`, carrying one `SetValue`
/// surface op, an optional `prompt`, and the given auth `token` (with `auth.from`
/// equal to `auth_from`). A `token`/`auth_from` mismatch with `from` is the
/// rejection path (VC-3.3).
fn drive_requested(
    at: u64,
    from: &PeerId,
    auth_from: &PeerId,
    token: &str,
    prompt: Option<&str>,
) -> Event {
    Event {
        origin: Origin::Peer,
        edge: PEER_EDGE,
        at,
        wall: None,
        input: LogicalInput::DriveRequested {
            from: from.clone(),
            envelope: PeerEnvelopeId(format!("env-{at}")),
            drive: DriveCommand {
                surface_ops: vec![SurfaceOp::SetValue {
                    surface: DRIVEN_SURFACE,
                    element: 2,
                    value: serde_json::json!("driven by peer"),
                    base_version: None,
                }],
                prompt: prompt.map(|s| s.to_string()),
            },
            auth: Authorization {
                from: auth_from.clone(),
                token: token.into(),
            },
        },
    }
}

/// Assert a recorded log folds and replays to a BYTE-IDENTICAL World with zero model
/// calls — the shared non-vacuous spine assertion (fold == replay, in bytes AND
/// value, and a second replay is deterministic). Returns the live-folded World for
/// further per-case assertions.
fn assert_byte_identical_replay(events: &[Event], model: &ModelConfig) -> World {
    // `drive_replay` takes NO ModelCaller, so this exploding client is structurally
    // unreachable from `replay_log`; the counter assertion documents zero calls.
    let exploding = ExplodingClient {
        calls: Arc::new(AtomicUsize::new(0)),
    };

    let live = fold_log(events, model).expect("the authored peer-drive log folds");
    let replay = replay_log(events, model)
        .expect("a faithful peer-drive replay REUSES every recorded result (never Diverged)");

    let live_bytes = serde_json::to_vec(&live).expect("serialize live World");
    let replay_bytes = serde_json::to_vec(&replay).expect("serialize replay World");
    assert_eq!(
        live_bytes, replay_bytes,
        "replay must reconstruct a BYTE-IDENTICAL World (Inv 6)"
    );
    assert_eq!(live, replay, "replay must reconstruct an equal World");

    let replay_again = replay_log(events, model).expect("second replay fold");
    assert_eq!(
        replay_bytes,
        serde_json::to_vec(&replay_again).expect("serialize second replay"),
        "two independent replays of the same log are byte-identical"
    );

    assert_eq!(
        exploding.calls.load(Ordering::SeqCst),
        0,
        "replay must invoke the model client zero times (Inv 6)"
    );
    live
}

// ---------------------------------------------------------------------------
// VC-3.1 (OUTBOUND) — a drive_peer turn → SendPeer → PeerSendOutcome replays
// ---------------------------------------------------------------------------

/// **VC-3.1 (sender)** a recorded peer-drive branch — a `drive_peer` tool turn that
/// emits `Command::SendPeer`, settled by the recorded `PeerSendOutcome`, then a
/// continuation turn — folds and replays BYTE-IDENTICALLY with zero model/peer sends.
/// The `SendPeer` is fingerprinted and its `PeerSendOutcome` reused from the cursor
/// (the SOLE dual): a wrong settle would re-fingerprint differently and `Diverged`,
/// so the byte-identity is NON-VACUOUS.
#[test]
fn outbound_peer_drive_branch_replays_byte_identical() {
    let model = offline_model();
    let to = peer("app-b");

    // tick 0 header offers the root `drive_peer`; tick 1 the human asks it to drive;
    // tick 2 the model answers with a `drive_peer` tool use (cmd 0 → opens the Peer
    // slot, mints SendPeer cmd 1); tick 3 the send is acknowledged Delivered (settles
    // the slot, mints the continuation CallModel cmd 2); tick 4 the turn ends.
    let mut events = vec![
        session_started(&[DRIVE_PEER]),
        user_message(1, "drive the peer app and ask it to confirm"),
        model_responded(
            2,
            0,
            vec![drive_peer_block("tu_drive", &to, Some("please confirm"))],
            StopReason::ToolUse,
        ),
        peer_send_outcome(3, 1, &to, DeliveryOutcome::Delivered),
        model_responded(
            4,
            2,
            vec![Block::Text {
                text: "the peer confirmed".into(),
            }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, &model);

    let live = assert_byte_identical_replay(&events, &model);

    // The Peer slot really settled: the turn assembled a SUCCESS `tool_result` paired
    // with the `drive_peer` tool_use, then ran a continuation to `EndTurn` (Idle).
    let entity = live.entities.get(&0).expect("primary entity present");
    assert!(
        matches!(entity.activity, Activity::Idle),
        "the drive turn settled to Idle after the continuation"
    );
    let has_drive_tool_result = entity.history.0.iter().any(|m| {
        matches!(m.role, Role::User)
            && m.content.iter().any(|b| {
                matches!(
                    b,
                    Block::ToolResult { tool_use_id, is_error, .. }
                        if tool_use_id == "tu_drive" && !is_error
                )
            })
    });
    assert!(
        has_drive_tool_result,
        "the recorded PeerSendOutcome::Delivered settled the Peer slot to a success tool_result"
    );
    let has_final_answer = entity.history.0.iter().any(|m| {
        matches!(m.role, Role::Assistant)
            && m.content.iter().any(|b| matches!(b, Block::Text { text } if text == "the peer confirmed"))
    });
    assert!(has_final_answer, "the continuation turn folded the final answer");
}

// ---------------------------------------------------------------------------
// VC-3.1 (INBOUND) — DriveRequested projects surface + enqueues prompt, replays
// ---------------------------------------------------------------------------

/// **VC-3.1 (target)** a recorded inbound `DriveRequested` (valid auth) applies its
/// `surface_ops` as a PROJECTION of that input (no separate `SurfaceMutated`, Inv 18)
/// and enqueues its `prompt` to the Inbox; a following initiation drains the parked
/// prompt and the target processes it. The folded and replayed Worlds are
/// byte-identical with zero model calls.
#[test]
fn inbound_drive_projects_surface_and_enqueues_prompt_replays_byte_identical() {
    let model = offline_model();
    let lead = peer("app-lead");

    // tick 0 header (no surface tools on the target); tick 1 binds the peer edge;
    // tick 2 the inbound drive (Origin::Peer) projects the SetValue and enqueues the
    // prompt; tick 3 a benign initiation drains the parked prompt FIRST and starts the
    // turn (cmd 0); tick 4 the target answers.
    let mut events = vec![
        session_started(&[]),
        edge_bound_peer(1, &lead),
        drive_requested(2, &lead, &lead, "app-lead", Some("Reply with exactly one word: ACK.")),
        user_message(3, "proceed"),
        model_responded(
            4,
            0,
            vec![Block::Text {
                text: "ACK".into(),
            }],
            StopReason::EndTurn,
        ),
    ];
    stamp_fingerprints(&mut events, &model);

    let live = assert_byte_identical_replay(&events, &model);

    // The drive's surface op applied as a projection (no separate SurfaceMutated):
    // the driven surface bumped 0→1 exactly once.
    assert_eq!(
        live.resources.surfaces.get(&DRIVEN_SURFACE).map(|s| s.version),
        Some(1),
        "the inbound drive's surface_ops apply as a one-fact projection (version 0→1)"
    );
    // There is NO agent/peer-origin SurfaceMutated in the log — the DriveRequested
    // entry is the SOLE record of the surface write (Inv 18).
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.input, LogicalInput::SurfaceMutated { .. }))
            .count(),
        0,
        "no separate SurfaceMutated is authored for the inbound drive (Inv 18)"
    );

    // The drive's prompt was honoured: it was drained from the Inbox into History (as
    // the FIRST user message, before the initiating message), the Inbox is now empty,
    // and the target produced a real answer.
    let entity = live.entities.get(&0).expect("primary entity present");
    assert!(
        entity.inbox.pending.is_empty(),
        "the parked prompt was drained from the Inbox on initiation"
    );
    let prompt_first = matches!(
        entity.history.0.first(),
        Some(Msg { role: Role::User, content })
            if content.iter().any(|b| matches!(b, Block::Text { text } if text.contains("ACK")))
    );
    assert!(
        prompt_first,
        "the drive's prompt was processed FIRST (drained from the Inbox ahead of the kick)"
    );
    assert!(
        matches!(entity.activity, Activity::Idle),
        "the target turn settled to Idle"
    );
}

// ---------------------------------------------------------------------------
// VC-3.3 — an invalid/absent Authorization is rejected (no mutation, no enqueue)
// ---------------------------------------------------------------------------

/// **VC-3.3** an inbound `DriveRequested` carrying an ABSENT (empty token) OR a
/// MISMATCHED (`auth.from != from`) `Authorization` is REJECTED by the inbound
/// transition: no surface mutation, no prompt enqueued. The World after the invalid
/// drive equals the World folded WITHOUT it (the drive is a logged no-op), proving
/// the rejection-invariance is non-vacuous, and the log still replays byte-identically.
#[test]
fn inbound_drive_with_invalid_auth_is_rejected() {
    let model = offline_model();
    let lead = peer("app-lead");
    let imposter = peer("app-imposter");

    // The baseline: the World folded with NO drive at all (just the header + binding).
    let baseline_events = vec![session_started(&[]), edge_bound_peer(1, &lead)];
    let baseline = fold_log(&baseline_events, &model).expect("baseline folds");

    // Case A — ABSENT authorization (empty token): the drive is logged but applies
    // nothing. The folded World must equal the baseline (the drive was a no-op).
    let empty_token_events = vec![
        session_started(&[]),
        edge_bound_peer(1, &lead),
        drive_requested(2, &lead, &lead, "", Some("do not run this")),
    ];
    let rejected_a = assert_byte_identical_replay(&empty_token_events, &model);
    assert!(
        rejected_a.resources.surfaces.is_empty(),
        "an empty-token drive mutates no surface (VC-3.3)"
    );
    assert!(
        rejected_a
            .entities
            .get(&0)
            .expect("root")
            .inbox
            .pending
            .is_empty(),
        "an empty-token drive enqueues no prompt (VC-3.3)"
    );
    // The only difference from the baseline is the extra logged input (DriveRequested
    // at tick 2 advanced the clock); the SETTLED state — entities, surfaces, edges — is
    // byte-identical to the baseline, so the invalid drive changed nothing observable.
    assert_eq!(
        rejected_a.entities, baseline.entities,
        "an empty-token drive leaves every entity unchanged (no prompt, no turn)"
    );
    assert_eq!(
        rejected_a.resources.surfaces, baseline.resources.surfaces,
        "an empty-token drive leaves the surface view unchanged"
    );

    // Case B — MISMATCHED authorization (`auth.from` asserts a different sender than
    // the envelope's `from`): likewise rejected, no mutation, no enqueue.
    let mismatched_events = vec![
        session_started(&[]),
        edge_bound_peer(1, &lead),
        drive_requested(2, &lead, &imposter, "app-imposter", Some("do not run this")),
    ];
    let rejected_b = assert_byte_identical_replay(&mismatched_events, &model);
    assert!(
        rejected_b.resources.surfaces.is_empty(),
        "a mismatched-auth drive mutates no surface (VC-3.3)"
    );
    assert!(
        rejected_b
            .entities
            .get(&0)
            .expect("root")
            .inbox
            .pending
            .is_empty(),
        "a mismatched-auth drive enqueues no prompt (VC-3.3)"
    );
    assert_eq!(
        rejected_b.entities, baseline.entities,
        "a mismatched-auth drive leaves every entity unchanged"
    );
}
