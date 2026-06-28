//! The FINAL verification sink for durable peer delivery (Area B): NON-VACUOUS,
//! DETERMINISTIC, OFFLINE proofs — over the REAL broker/mailbox/reconstruction with
//! temp dirs and ZERO live model calls — that a peer message is neither lost nor
//! double-applied across a crash (US-5/US-6; VC-4.1/4.2/4.3). Durability is
//! model-independent, so these tests need no credentials and run on every machine.
//!
//! It composes the production durable primitives directly (no `src/` patch):
//!
//! - **FSYNC-BEFORE-DELIVER + DELIVERED-AFTER-ACK (VC-4.1)** — a durable
//!   `PeerBroker::relay` to a LIVE sink fsyncs the `DurableEnvelope` into the
//!   receiver's `inbox.jsonl` BEFORE it touches the sink. The proof reads the raw
//!   inbox file off disk at the instant the frame is observed (the envelope is
//!   durably present), and asserts the relay has NOT yet reported `Delivered` while
//!   it awaits the ack — `Delivered` is reported ONLY after `confirm_fold`, after
//!   which the durable inbox is advanced empty. NON-VACUOUS: an optimistic
//!   `Delivered`-on-`sink.send` would finish before the confirm and leave the
//!   envelope un-advanced.
//! - **QUEUED PERSISTS + OUTBOX RECONSTRUCTION ACROSS RESTART (VC-4.1)** — without a
//!   durable fold (an OFFLINE target) the outcome is `Queued` and the envelope
//!   PERSISTS in the durable inbox, surviving a simulated peer restart (a fresh
//!   `Mailbox` reopened over the same dir still finds it). A real `reconstruct_outbox`
//!   over a synthetic sender log proves the outbox is exactly `{Queued} − {Delivered}`:
//!   a never-`Delivered` `Queued` send is outstanding (redelivered on restart) while a
//!   `Queued`-then-`Delivered` send is excluded; once acked it leaves the outbox.
//! - **REDELIVERY DEDUP TO TRACE (VC-4.2)** — a redelivered `DriveRequested`
//!   (same `PeerEnvelopeId`) folded through the pure `tick` reducer is a NO-OP at the
//!   receiver's stratum-1 `applied_envelopes`: the World is BYTE-UNCHANGED (no second
//!   surface bump, no second prompt) — effectively-once. NON-VACUOUS: a broken dedup
//!   would re-apply and the bytes would differ.
//! - **REJECTNEWEST FULL INBOX (VC-4.3)** — a full `peer_inbox` (via
//!   `with_peer_inbox_cap`) returns `DeliveryOutcome::Rejected` to the sender with
//!   NOTHING enqueued: the durable inbox depth is unchanged and the rejected envelope
//!   is absent. NON-VACUOUS: a broken cap would enqueue and grow the depth.
//!
//! See docs/agent/world/ecs-runtime.md §"Durable peer delivery" — INV-3
//! (fsync-before-deliver), Delivered-after-durable-fold, INV-4 (advance-after-fold),
//! stratum-1 dedup (`applied_envelopes`), the RejectNewest peer-inbox cap, and the
//! OUTBOX RECONSTRUCTION rule (`reconstruct_outbox`).

use std::sync::Arc;

use serde_json::Value as Json;

use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{
    Authorization, DeliveryOutcome, DriveCommand, DurableEnvelope, Event, Fingerprint,
    LogicalInput, Origin, PeerPayload,
};
use rubberdux::agent::world::surface::{PeerEnvelopeId, SurfaceOp};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Counterpart, Effort, Identity, Inbox, Lineage, ModelConfig,
    PeerId as WorldPeerId, Resources, World,
};
use rubberdux::app::AppId;
use rubberdux::app::peer::PeerId as AppPeerId;
use rubberdux::app::peer::mailbox::{Mailbox, PeerEnvelope};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::host::{OutboxKey, PeerBroker, PeerRouteOutcome, reconstruct_outbox};
use rubberdux::protocol::HostToAgent;

// The peer edge an inbound `DriveRequested` binds to (the human edge is 0). Inert to
// the surface projection / prompt enqueue under test, but authored to mirror the
// driver's real log shape.
const PEER_EDGE: u32 = 1;

// The surface the inbound drive targets. Bumped 0→1 by a single projected `SetValue`.
const DRIVEN_SURFACE: u32 = 9;

// ---------------------------------------------------------------------------
// Shared fixtures: stores, peer ids, durable wire payloads
// ---------------------------------------------------------------------------

/// A filesystem-backed App store rooted at a fresh temp dir — the broker consults it
/// only for a target's on-disk home when queueing to its durable inbox. The dir is
/// detached (`into_path`) so the store owns it for the test's lifetime, mirroring the
/// `host.rs` broker tests.
fn store() -> Arc<dyn AppStore> {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    Arc::new(FilesystemAppStore::with_apps_dir(dir))
}

/// An App-side peer id on the local node — the broker's `from`/`to`.
fn app_peer(app: &str) -> AppPeerId {
    AppPeerId::local(AppId(app.into()))
}

/// A World-side peer id on the local node — the `to`/`auth.from` carried in the
/// durable envelope and the synthetic `PeerSendOutcome` log.
fn world_peer(app: &str) -> WorldPeerId {
    WorldPeerId {
        app_id: app.into(),
        node_id: "local".into(),
    }
}

/// The receiver's durable inbox (`inbox.jsonl`), resolved from the store the same way
/// the broker resolves it — what `peek`/`depth` reads to observe the fsynced queue.
fn receiver_mailbox(store: &Arc<dyn AppStore>, app: &str) -> Mailbox {
    Mailbox::in_dir(&store.app_dir(&AppId(app.into())))
}

/// A `DurableEnvelope` wire payload carrying envelope id `id` — the durable peer path
/// the broker fsyncs, delivers, and dedups by. Mirrors what `BrokerPeerSender` puts on
/// the wire so the receiver reconstructs the inbound peer input (id + payload + auth).
fn durable_wire(id: &str) -> Json {
    let durable = DurableEnvelope {
        id: PeerEnvelopeId(id.into()),
        payload: PeerPayload::Message(serde_json::json!({ "text": "x" })),
        auth: Authorization {
            from: world_peer("a"),
            token: "a".into(),
        },
    };
    serde_json::to_value(&durable).expect("serialise durable envelope")
}

// ---------------------------------------------------------------------------
// VC-4.1 (part 1) — fsync-before-deliver + Delivered-only-after-durable-fold
// ---------------------------------------------------------------------------

/// **VC-4.1** the durable relay fsyncs the envelope into the receiver's durable inbox
/// BEFORE delivery, and reports `Delivered` ONLY after a receiver durable-fold
/// confirmation — never on the bare `sink.send` Ok. The two halves are both
/// NON-VACUOUS: the envelope is read off disk at the instant the frame is observed
/// (fsync-before-deliver), and the relay is asserted UNFINISHED while it awaits the
/// ack (Delivered-after-ack) — an optimistic `Delivered` would have finished already.
/// On confirmation the durable inbox is advanced empty (INV-4).
#[tokio::test]
async fn fsync_before_deliver_then_delivered_only_after_durable_fold() {
    let store = store();
    let broker = Arc::new(PeerBroker::new(store.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    broker.register(app_peer("b"), tx).await;

    // Drive the relay concurrently: it fsyncs the envelope, delivers to the live
    // sink, then BLOCKS awaiting the durable-fold confirmation (no confirm yet).
    let relay_broker = broker.clone();
    let handle = tokio::spawn(async move {
        relay_broker
            .relay(app_peer("a"), app_peer("b"), durable_wire("env-1"))
            .await
    });

    // The frame reached the live sink: delivery HAS been attempted.
    let frame = rx.recv().await.expect("the durable relay delivers to the live sink");
    match frame {
        HostToAgent::PeerDeliver { from, payload } => {
            assert_eq!(from, "a", "the broker stamps the sending App id");
            let env: DurableEnvelope =
                serde_json::from_value(payload).expect("the relayed payload is a DurableEnvelope");
            assert_eq!(env.id, PeerEnvelopeId("env-1".into()));
        }
        other => panic!("expected PeerDeliver, got {other:?}"),
    }

    // FSYNC-BEFORE-DELIVER (non-vacuous): at the instant the frame is observed the
    // envelope is ALREADY durable on disk — a fresh Mailbox reopened over the same
    // dir finds it, and the raw inbox.jsonl physically contains the id. A relay that
    // delivered before fsyncing would leave the file empty/absent here.
    let mailbox = receiver_mailbox(&store, "b");
    let queued = mailbox.peek().expect("peek b's durable inbox at delivery time");
    assert_eq!(queued.len(), 1, "the envelope is durably queued BEFORE delivery");
    assert_eq!(
        queued[0].durable().map(|d| d.id),
        Some(PeerEnvelopeId("env-1".into())),
        "the fsynced envelope is exactly the one being delivered"
    );
    let raw = std::fs::read_to_string(mailbox.path()).expect("read the durable inbox file");
    assert!(
        raw.contains("env-1"),
        "the envelope is physically on disk (fsync-before-deliver), not merely in memory"
    );

    // Let the relay task run up to its await point, then assert it has NOT reported
    // a verdict yet: `Delivered` is GATED on the durable-fold ack, which has not
    // arrived. NON-VACUOUS — an optimistic `Delivered`-on-`sink.send` would already
    // have finished the task here.
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "the relay must NOT report a verdict before the durable-fold ack (Delivered-after-ack)"
    );

    // The receiver durably folded the envelope (its stratum-1 World log appended the
    // peer input): confirm it → the relay may now report `Delivered` and advance.
    broker.confirm_fold(&PeerEnvelopeId("env-1".into())).await;
    let outcome = handle
        .await
        .expect("the relay task joins")
        .expect("the relay completes Ok");
    assert_eq!(
        outcome,
        PeerRouteOutcome::Delivered,
        "Delivered is reported only AFTER the durable-fold confirmation"
    );

    // INV-4: the durable inbox is advanced past the now-folded envelope.
    assert!(
        receiver_mailbox(&store, "b").is_empty(),
        "the durable inbox is advanced empty after the confirmed fold"
    );
}

// ---------------------------------------------------------------------------
// VC-4.1 (part 2) — without an ack the outcome is Queued and it PERSISTS
// ---------------------------------------------------------------------------

/// **VC-4.1** WITHOUT a durable fold (an OFFLINE target — no live sink, no ack) the
/// durable relay reports `Queued`, NOT `Delivered`, and the envelope PERSISTS in the
/// durable, reconstructable inbox: it survives a simulated peer restart (a fresh
/// `Mailbox` reopened over the same dir still holds it). NON-VACUOUS — a relay that
/// reported `Delivered` here, or dropped the envelope, would fail both assertions.
#[tokio::test]
async fn without_durable_fold_ack_the_outcome_is_queued_and_persists_across_restart() {
    let store = store();
    let broker = PeerBroker::new(store.clone());
    // No sink registered for `b`: it is offline (tombstoned or archived).
    let outcome = broker
        .relay(app_peer("a"), app_peer("b"), durable_wire("env-2"))
        .await
        .expect("the relay completes Ok");
    assert_eq!(
        outcome,
        PeerRouteOutcome::Queued { wake: app_peer("b") },
        "an offline target with no durable fold is Queued, never Delivered"
    );

    // The envelope is durable in the inbox queue (fsynced on the accept path).
    let queued = receiver_mailbox(&store, "b")
        .peek()
        .expect("peek b's durable inbox");
    assert_eq!(queued.len(), 1, "the un-acked envelope persists in the durable inbox");
    assert_eq!(
        queued[0].durable().map(|d| d.id),
        Some(PeerEnvelopeId("env-2".into()))
    );

    // SIMULATED PEER RESTART: a fresh Mailbox over the SAME on-disk dir (a brand-new
    // process view) still finds the envelope — the durable queue is reconstructed
    // from disk, not held in the broker's memory.
    let reopened = receiver_mailbox(&store, "b");
    let surviving = reopened.peek().expect("re-peek after a simulated restart");
    assert_eq!(
        surviving.len(),
        1,
        "the queued envelope survives a peer restart (durable, reconstructable)"
    );
    assert_eq!(
        surviving[0].durable().map(|d| d.id),
        Some(PeerEnvelopeId("env-2".into()))
    );
}

// ---------------------------------------------------------------------------
// VC-4.1 (part 2) — outbox reconstruction is {Queued} − {Delivered}; restart retries
// ---------------------------------------------------------------------------

/// A sender-local `PeerSendOutcome` — the durable World-log record the outbox is
/// reconstructed from. The outbox identity is `(to, fingerprint)` (content-addressed),
/// NOT `cmd`, so a retry of the same logical send reconciles against its first record.
fn peer_send_outcome(cmd: CmdId, to: &str, fp: &str, outcome: DeliveryOutcome) -> LogicalInput {
    LogicalInput::PeerSendOutcome {
        cmd,
        entity: 0,
        fingerprint: Fingerprint(fp.into()),
        to: world_peer(to),
        outcome,
    }
}

/// **VC-4.1** a real `reconstruct_outbox` over a synthetic sender log proves the
/// reconstructed outbox is exactly `{Queued} − {later Delivered}`: a never-`Delivered`
/// `Queued` send (to B) is OUTSTANDING and retried on restart; a `Queued`-then-
/// `Delivered` send (to C) is EXCLUDED. The outstanding envelope persists in B's
/// durable inbox across a simulated restart, is redelivered, and — once the receiver
/// folds it — is advanced out (effectively-once, never re-applied). Once the sender's
/// log records the `Delivered` the send leaves the outbox (no re-retry). NON-VACUOUS:
/// a broken reconstruction would include the Delivered send or drop the Queued one.
#[test]
fn queued_send_is_reconstructed_into_the_outbox_and_retried_across_restart() {
    // The sender log: B's send was only ever Queued; C's was Queued then Delivered
    // (a retry that succeeded). A bare UserMessage is present to prove non-send
    // inputs are ignored by the reconstruction.
    let sender_log = vec![
        LogicalInput::UserMessage {
            to: 0,
            text: "ignored by the outbox".into(),
        },
        peer_send_outcome(1, "B", "fp-b", DeliveryOutcome::Queued),
        peer_send_outcome(2, "C", "fp-c", DeliveryOutcome::Queued),
        peer_send_outcome(3, "C", "fp-c", DeliveryOutcome::Delivered),
    ];
    let outbox = reconstruct_outbox(sender_log.iter());

    // Exactly B's never-Delivered Queued send is outstanding.
    assert_eq!(outbox.len(), 1, "only the still-Queued send is outstanding");
    let key_b = OutboxKey {
        to: world_peer("B"),
        fingerprint: "fp-b".into(),
    };
    let entry = outbox.get(&key_b).expect("B's queued send is in the reconstructed outbox");
    assert_eq!(entry.to, world_peer("B"), "the outbox names B as the destination to re-drain");
    assert_eq!(entry.cmd, 1, "the outstanding send's originating command is reconstructed");
    // A Queued-then-Delivered envelope (C) is EXCLUDED (a later Delivered supersedes
    // the earlier Queued) — a broken reconstruction that kept it would fail here.
    assert!(
        !outbox.contains_key(&OutboxKey {
            to: world_peer("C"),
            fingerprint: "fp-c".into(),
        }),
        "a Queued-then-Delivered send is excluded from the reconstructed outbox"
    );

    // The outstanding envelope persists in B's durable inbox (the broker fsynced it
    // before the failed delivery) — what the restore-drain redelivers.
    let dir = tempfile::tempdir().expect("tempdir for B's durable inbox");
    let mailbox = Mailbox::in_dir(dir.path());
    let env_id = PeerEnvelopeId("fp-b".into());
    let durable = DurableEnvelope {
        id: env_id.clone(),
        payload: PeerPayload::Message(serde_json::json!({ "text": "drive B" })),
        auth: Authorization {
            from: world_peer("A"),
            token: "A".into(),
        },
    };
    let envelope = PeerEnvelope {
        from: app_peer("A"),
        payload: serde_json::to_value(&durable).expect("serialize durable envelope"),
    };
    mailbox
        .enqueue_unique(&envelope, &env_id)
        .expect("fsync the outstanding envelope into B's durable inbox");

    // SIMULATED PEER RESTART: a fresh Mailbox over the SAME dir still holds the
    // outstanding envelope — the restart retries it from the durable queue.
    let reopened = Mailbox::in_dir(dir.path());
    let redelivered = reopened.peek().expect("peek B's inbox after restart");
    assert_eq!(
        redelivered.len(),
        1,
        "the outstanding queued envelope is redelivered on peer restart"
    );
    assert_eq!(redelivered[0].durable().map(|d| d.id), Some(env_id.clone()));

    // ONCE FOLDED → NOT re-applied: after the receiver durably folds it the drain
    // advances past it, so a subsequent restart finds nothing — effectively-once.
    assert!(
        reopened.advance(&env_id).expect("advance past the folded envelope"),
        "the folded envelope is advanced out of the durable inbox"
    );
    assert!(
        reopened.peek().expect("re-peek after advance").is_empty(),
        "an already-folded envelope is not redelivered on a later restart"
    );

    // Once the sender's log records the Delivered, the send leaves the reconstructed
    // outbox (the durable outbox and the receiver's fold agree — no re-retry).
    let mut acked_log = sender_log.clone();
    acked_log.push(peer_send_outcome(4, "B", "fp-b", DeliveryOutcome::Delivered));
    assert!(
        reconstruct_outbox(acked_log.iter()).is_empty(),
        "once Delivered, the outstanding send leaves the reconstructed outbox"
    );
}

// ---------------------------------------------------------------------------
// VC-4.2 — a redelivered envelope is deduped to a trace NO-OP (effectively-once)
// ---------------------------------------------------------------------------

/// The fresh `World` the dedup proof folds from: tick 0, a single primary `Idle`
/// root entity, empty surfaces and an empty stratum-1 `applied_envelopes` set.
fn genesis() -> World {
    let model = ModelConfig {
        model: "claude-peer-durability".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    };
    let mut world = World::new(0, Resources::new(7, model));
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
        },
    );
    world
}

/// An `EdgeBound` binding the peer edge to `Counterpart::Peer(from)` (System-origin),
/// mirroring what the driver logs before the first inbound peer input.
fn edge_bound_peer(at: u64, from: &WorldPeerId) -> Event {
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
/// surface op, a `prompt`, and a VALID authorization (`auth.from == from`, non-empty
/// token) so the first fold applies. `env` is the stratum-1 dedup key.
fn drive_requested(at: u64, from: &WorldPeerId, env: &str, prompt: &str) -> Event {
    Event {
        origin: Origin::Peer,
        edge: PEER_EDGE,
        at,
        wall: None,
        input: LogicalInput::DriveRequested {
            from: from.clone(),
            envelope: PeerEnvelopeId(env.into()),
            drive: DriveCommand {
                surface_ops: vec![SurfaceOp::SetValue {
                    surface: DRIVEN_SURFACE,
                    element: 2,
                    value: serde_json::json!("driven by peer"),
                    base_version: None,
                }],
                prompt: Some(prompt.into()),
            },
            auth: Authorization {
                from: from.clone(),
                token: from.app_id.clone(),
            },
        },
    }
}

/// **VC-4.2** a redelivered `DriveRequested` whose `PeerEnvelopeId` was ALREADY folded
/// is deduped at the receiver's stratum-1 `applied_envelopes` to a TRACE NO-OP: the
/// second fold emits no Commands and leaves the World BYTE-UNCHANGED — the surface is
/// NOT bumped a second time, the prompt is NOT enqueued twice, the dedup set still
/// holds the envelope exactly once. NON-VACUOUS: the first fold demonstrably APPLIES
/// (so the no-op is not vacuously "nothing ever happens"), and a broken dedup would
/// re-apply (surface version 2, two queued prompts) and the serialized bytes would
/// differ — at-least-once redelivery folds to effectively-once.
#[test]
fn redelivered_envelope_is_deduped_to_a_trace_noop() {
    let lead = world_peer("app-lead");

    // Bind the peer edge, then fold the inbound drive ONCE.
    let bound = tick(&genesis(), &edge_bound_peer(1, &lead)).0;
    let drive = drive_requested(2, &lead, "env-1", "please confirm");
    let (once, c1) = tick(&bound, &drive);

    // The FIRST fold genuinely APPLIED: surface 9 bumped 0→1 (a projection — no
    // separate SurfaceMutated), the prompt enqueued, the envelope recorded in the
    // stratum-1 dedup set. This makes the redelivery no-op non-vacuous.
    assert!(c1.is_empty(), "an inbound drive emits no Commands");
    assert_eq!(
        once.resources.surfaces.get(&DRIVEN_SURFACE).map(|s| s.version),
        Some(1),
        "the first fold projects the surface op (version 0→1)"
    );
    assert_eq!(
        once.entities.get(&0).expect("root").inbox.pending,
        vec![vec![Block::Text {
            text: "please confirm".into()
        }]],
        "the first fold enqueues the drive's prompt to the Inbox"
    );
    assert!(
        once.resources
            .applied_envelopes
            .contains(&PeerEnvelopeId("env-1".into())),
        "the first fold records the envelope in the stratum-1 dedup set"
    );

    // REDELIVERY of the SAME envelope (an at-least-once retry, or a crash-interrupted
    // earlier drain): a TRACE NO-OP — never re-applied.
    let (twice, c2) = tick(&once, &drive);
    assert!(c2.is_empty(), "a deduped redelivery emits no Commands");

    // The World is BYTE-UNCHANGED on the redelivery (value AND bytes).
    assert_eq!(
        &twice, &once,
        "a redelivered envelope is a NO-OP: the World is unchanged (effectively-once)"
    );
    assert_eq!(
        serde_json::to_vec(&twice).expect("serialize twice"),
        serde_json::to_vec(&once).expect("serialize once"),
        "a redelivered envelope leaves the World BYTE-identical"
    );

    // And, spelled out: no second surface bump, no second prompt, recorded once.
    assert_eq!(
        twice.resources.surfaces.get(&DRIVEN_SURFACE).map(|s| s.version),
        Some(1),
        "the surface version is NOT bumped a second time by the redelivery"
    );
    assert_eq!(
        twice.entities.get(&0).expect("root").inbox.pending.len(),
        1,
        "the prompt is NOT enqueued a second time by the redelivery"
    );
    assert_eq!(
        twice.resources.applied_envelopes.len(),
        1,
        "the envelope is recorded exactly once (deduped, not double-applied)"
    );
}

// ---------------------------------------------------------------------------
// VC-4.3 — a full peer_inbox (RejectNewest) returns Rejected, nothing enqueued
// ---------------------------------------------------------------------------

/// **VC-4.3** a full `peer_inbox` (RejectNewest, via `with_peer_inbox_cap`) returns
/// `DeliveryOutcome::Rejected` to the sender with NOTHING enqueued: the durable inbox
/// depth does not grow on the reject and the rejected envelope is absent from the
/// queue. NON-VACUOUS: a broken cap would enqueue the second send and the depth would
/// grow to 2.
#[tokio::test]
async fn full_peer_inbox_rejects_the_new_send_with_nothing_enqueued() {
    let store = store();
    // Cap = 1: a single queued envelope fills the durable inbox.
    let broker = PeerBroker::new(store.clone()).with_peer_inbox_cap(1);

    // First send to the offline target: the inbox is empty → enqueued (Queued).
    let first = broker
        .relay(app_peer("a"), app_peer("b"), durable_wire("env-cap-1"))
        .await
        .expect("first relay Ok");
    assert_eq!(first, PeerRouteOutcome::Queued { wake: app_peer("b") });
    assert_eq!(
        receiver_mailbox(&store, "b").depth().expect("depth after first"),
        1,
        "the first send lands in the durable inbox (now at cap)"
    );

    // Second send: the inbox is at cap → Rejected (RejectNewest); nothing enqueued.
    let second = broker
        .relay(app_peer("a"), app_peer("b"), durable_wire("env-cap-2"))
        .await
        .expect("second relay Ok");
    assert_eq!(
        second,
        PeerRouteOutcome::Rejected,
        "a full peer_inbox rejects the new send (RejectNewest)"
    );

    // The durable inbox is UNCHANGED on the reject — depth still 1, and the rejected
    // envelope was never written (non-vacuous: a broken cap would grow it to 2).
    let queued = receiver_mailbox(&store, "b").peek().expect("peek after reject");
    assert_eq!(queued.len(), 1, "nothing is enqueued on a RejectNewest reject");
    assert_eq!(
        queued[0].durable().map(|d| d.id),
        Some(PeerEnvelopeId("env-cap-1".into())),
        "only the first (pre-cap) envelope remains queued"
    );
    assert!(
        !queued
            .iter()
            .any(|e| e.durable().map(|d| d.id) == Some(PeerEnvelopeId("env-cap-2".into()))),
        "the rejected envelope is absent from the durable inbox"
    );
}
