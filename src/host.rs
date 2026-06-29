use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use teloxide::prelude::Bot;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};

use crate::agent::world::effects::{CommandKey, PeerSender};
use crate::agent::world::inputs::{
    Authorization, DeliveryOutcome, DurableEnvelope, LogicalInput, PeerPayload,
};
use crate::agent::world::surface::PeerEnvelopeId;
use crate::agent::world::world::{CmdId, EntityId, PeerId as WorldPeerId};
use crate::error::Error;
use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::vm::manager::VMManager;

const DEFAULT_RPC_PORT: u16 = 19384;

/// Default TCP port for the surface-client listener: the host endpoint the macOS
/// GUI client connects to so the host's [`SurfaceRouter`] can relay surface frames
/// between it and the App's worker subprocess. Distinct from the worker RPC port
/// ([`DEFAULT_RPC_PORT`] 19384, which routes worker `Hello`s) and the gateway HTTP
/// port (19385). Overridable via `RUBBERDUX_SURFACE_PORT`. The macOS client (W5)
/// dials this port and registers with an `AgentToHost::Hello { app_id }` frame.
const DEFAULT_SURFACE_PORT: u16 = 19386;

/// Bound on a surface client's outbound drive queue. Drives arrive at human
/// interaction speed; a small buffer absorbs bursts without unbounded growth.
const SURFACE_CLIENT_DRIVE_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Peer broker: the directory + relay (a switch, not an orchestrator)
// ---------------------------------------------------------------------------

/// A live delivery sink for one local App worker: the channel the broker writes a
/// resolved [`PeerDeliver`](crate::protocol::HostToAgent::PeerDeliver) frame into.
/// Held only while the worker is running; absent for a tombstoned or archived App,
/// for which the broker queues to the inbox instead. The broker treats the sink as
/// opaque — it forwards an envelope and makes no decision about its contents.
pub type PeerSink = tokio::sync::mpsc::Sender<crate::protocol::HostToAgent>;

/// The host's peer broker: a directory + relay that resolves a
/// [`PeerId`](crate::app::peer::PeerId) to a connection and forwards opaque
/// message envelopes. It is a **switch, not an orchestrator** (design decision
/// D5, `docs/app/peer/decentralized-messaging.md`): it makes no routing or
/// coordination decisions beyond "is this target's worker live right now?".
///
/// - If the target peer has a live sink, the broker delivers the envelope to it
///   directly (the directory is refreshed on every such delivery, keeping the
///   most-recently-used ordering current).
/// - Otherwise the target is offline (tombstoned or human-archived — both are
///   still addressable): the broker queues the envelope to the target's
///   `inbox.jsonl` and signals that the App should be woken, so a restore drains
///   it. Archiving is human-facing and does **not** gate addressability.
///
/// The same resolution works for a remote peer once the broker federates over the
/// general TCP transport: a non-local [`PeerId`] is forwarded to the node that
/// owns it. That path is not wired here, but the addressing model already carries
/// the node identity so adding it later changes no call site.
pub struct PeerBroker {
    store: Arc<dyn crate::app::registry::store::AppStore>,
    directory: Mutex<crate::app::peer::directory::PeerDirectory>,
    /// Live delivery sinks keyed by peer id. A present entry means the peer's
    /// worker is running and can be delivered to directly.
    sinks: Mutex<HashMap<crate::app::peer::PeerId, PeerSink>>,
    /// Receiver durable-fold confirmations, by [`PeerEnvelopeId`]. A durable relay
    /// awaits one here before reporting `Delivered` (INV-3): the receiver signals it
    /// (via [`PeerBroker::confirm_fold`], driven by an `AgentToHost::PeerDeliverAck`
    /// the receiver emits AFTER appending the folded peer input to its stratum-1
    /// World log). The confirmation is the SOLE basis for `Delivered` and for
    /// advancing the durable inbox — never a bare `sink.send` Ok.
    folds: Mutex<FoldRegistry>,
    /// How long a durable relay waits for the receiver's durable-fold confirmation
    /// before falling back to `Queued` (the envelope PERSISTS in the durable inbox,
    /// retried on restore). Bounded so the relay never blocks indefinitely.
    fold_ack_timeout: Duration,
    /// Maximum depth of any target's durable peer inbox before a new send is
    /// rejected with RejectNewest: the incoming envelope is NOT enqueued and the
    /// sender receives `PeerRouteOutcome::Rejected`. Mirrors `Caps.peer_inbox`
    /// (world.rs). `0` = unbounded (no backpressure). See
    /// docs/agent/world/ecs-runtime.md §"Durable peer delivery" (§1335–1338).
    peer_inbox_cap: u32,
}

/// Default bound on how long a durable relay awaits the receiver's durable-fold
/// confirmation before reporting `Queued`. Generous enough for a healthy
/// cross-process fold roundtrip; on timeout the envelope is not lost — it stays in
/// the durable inbox for the restore-drain to redeliver.
const DEFAULT_FOLD_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// The receiver durable-fold confirmation registry: which envelopes the receiver
/// has durably folded, and which relays are currently awaiting that confirmation.
/// The confirmation flows from the RECEIVER (its stratum-1 World log appended the
/// peer input) back to the broker, making the sender's `Delivered` non-optimistic
/// (INV-3) and gating the durable-inbox advance (INV-4). A `BTreeSet`/`BTreeMap`
/// (never a `HashMap`) keeps the durable path free of a hidden ordering input.
#[derive(Default)]
struct FoldRegistry {
    /// Envelopes the receiver has confirmed a durable fold for but no relay is
    /// currently awaiting — a confirm that raced ahead of, or outlived, its relay.
    /// A subsequent `await_fold` consumes the pre-confirmation without blocking.
    confirmed: BTreeSet<PeerEnvelopeId>,
    /// Relays currently awaiting a confirm, keyed by envelope id. `confirm_fold`
    /// resolves the matching waiter; `await_fold` removes its own on timeout.
    waiters: BTreeMap<PeerEnvelopeId, oneshot::Sender<()>>,
}

/// What the broker decided for one `PeerSend`: either it was delivered to a live
/// worker, or it was queued to the offline target's inbox (and the target should
/// be woken so a restore drains it). Returned so the caller — the supervisor —
/// performs the side effect it owns (restoring the App) without the broker
/// reaching into supervision: the broker stays a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerRouteOutcome {
    /// The envelope was handed to the target's live delivery sink.
    Delivered,
    /// The target's worker was not running; the envelope was queued to its inbox.
    /// The supervisor should restore the target so the inbox is drained to it.
    Queued { wake: crate::app::peer::PeerId },
    /// The sender tried to address a peer it may not (itself); nothing was sent.
    Rejected,
}

impl PeerBroker {
    /// A broker backed by the given App store, with an empty directory and no
    /// live sinks. The store is consulted only for an App's on-disk directory
    /// (`app_dir`) when queueing to an inbox; routing decisions stay minimal.
    pub fn new(store: Arc<dyn crate::app::registry::store::AppStore>) -> Self {
        Self {
            store,
            directory: Mutex::new(crate::app::peer::directory::PeerDirectory::new()),
            sinks: Mutex::new(HashMap::new()),
            folds: Mutex::new(FoldRegistry::default()),
            fold_ack_timeout: DEFAULT_FOLD_ACK_TIMEOUT,
            peer_inbox_cap: 0,
        }
    }

    /// The same broker with a custom durable-fold-ack timeout. Used by tests to
    /// drive the `Queued`-on-no-confirm path without waiting the production default.
    #[cfg(test)]
    pub fn with_fold_ack_timeout(mut self, timeout: Duration) -> Self {
        self.fold_ack_timeout = timeout;
        self
    }

    /// The same broker with a bounded peer inbox cap (RejectNewest). When the
    /// target's durable inbox depth is at `cap`, a new send is rejected — nothing
    /// is enqueued and the caller receives `PeerRouteOutcome::Rejected`. `0` leaves
    /// the inbox unbounded (the default). Mirrors `Caps.peer_inbox`. See
    /// docs/agent/world/ecs-runtime.md §"Durable peer delivery" (§1335–1338).
    pub fn with_peer_inbox_cap(mut self, cap: u32) -> Self {
        self.peer_inbox_cap = cap;
        self
    }

    /// Record that the RECEIVER has durably folded the envelope `id` — its
    /// stratum-1 World log appended the corresponding `DriveRequested`/
    /// `PeerDelivered`. This is the receiver→broker durable-fold confirmation that
    /// makes the sender's `Delivered` non-optimistic (INV-3) and lets the broker
    /// advance the durable inbox (INV-4). Called by the supervisor's per-worker pump
    /// on an `AgentToHost::PeerDeliverAck`. Idempotent: a confirm with no waiting
    /// relay is remembered so a later (retried) relay's await returns at once — a
    /// redelivery of an already-folded envelope is still confirmable, never stuck.
    pub async fn confirm_fold(&self, id: &PeerEnvelopeId) {
        let mut folds = self.folds.lock().await;
        match folds.waiters.remove(id) {
            // A relay is awaiting this fold: resolve it. An `Err` means the relay
            // already gave up (timed out) — harmless; remember it for a retry below.
            Some(tx) => {
                if tx.send(()).is_err() {
                    folds.confirmed.insert(id.clone());
                }
            }
            // The confirm raced ahead of (or outlived) its relay: remember it so the
            // next await for this id returns immediately.
            None => {
                folds.confirmed.insert(id.clone());
            }
        }
    }

    /// Await the receiver's durable-fold confirmation for `id`, bounded by
    /// `fold_ack_timeout`. Returns `true` once the receiver confirms a durable fold
    /// (so the broker may report `Delivered` and advance the inbox), `false` on
    /// timeout (the envelope stays queued for the restore-drain → `Queued`). A
    /// pre-recorded confirmation (a confirm that arrived first) returns immediately.
    /// The restore-drain (`drain_peer_inbox`) awaits the same confirmation before
    /// advancing the durable inbox past a redelivered envelope (INV-4).
    pub async fn await_fold(&self, id: &PeerEnvelopeId) -> bool {
        let rx = {
            let mut folds = self.folds.lock().await;
            if folds.confirmed.remove(id) {
                return true;
            }
            let (tx, rx) = oneshot::channel();
            folds.waiters.insert(id.clone(), tx);
            rx
        };
        match tokio::time::timeout(self.fold_ack_timeout, rx).await {
            Ok(Ok(())) => true,
            // Sender dropped without confirming, or the wait timed out: not folded.
            // Clear any stale waiter so a later confirm is simply remembered.
            Ok(Err(_)) | Err(_) => {
                self.folds.lock().await.waiters.remove(id);
                false
            }
        }
    }

    /// Register a live worker's delivery sink and admit it to the directory,
    /// called when an App's worker becomes active. Makes the App both reachable
    /// (listed by `peer_list`) and directly deliverable.
    pub async fn register(&self, id: crate::app::peer::PeerId, sink: PeerSink) {
        self.directory.lock().await.register(id.clone());
        self.sinks.lock().await.insert(id, sink);
    }

    /// Remove a worker's sink and drop it from the directory, called when the
    /// App's worker stops (tombstone/suspend/archive). The App stays addressable:
    /// a later `PeerSend` to it is queued to its inbox.
    pub async fn unregister(&self, id: &crate::app::peer::PeerId) {
        self.sinks.lock().await.remove(id);
        self.directory.lock().await.unregister(id);
    }

    /// The peers `from` may address right now, in most-recently-used order. The
    /// answer to a worker's `PeerList`. Refreshes `from`'s own recency since
    /// asking is itself activity.
    pub async fn list_for(&self, from: &crate::app::peer::PeerId) -> Vec<crate::app::peer::PeerId> {
        let mut directory = self.directory.lock().await;
        directory.touch(from);
        directory.addressable_by(from)
    }

    /// Relay one peer message from `from` to `to`. The broker's whole job: resolve
    /// `to` to a live sink and deliver, or queue to its inbox if offline. It makes
    /// no decision about the payload — it forwards the envelope verbatim. Returns
    /// the [`PeerRouteOutcome`] so the supervisor performs any wake it owns.
    pub async fn relay(
        &self,
        from: crate::app::peer::PeerId,
        to: crate::app::peer::PeerId,
        payload: serde_json::Value,
    ) -> Result<PeerRouteOutcome, Error> {
        // The single policy gate: an App may not address itself.
        {
            let directory = self.directory.lock().await;
            if !directory.may_send(&from, &to) {
                return Ok(PeerRouteOutcome::Rejected);
            }
        }

        // Sending is activity: refresh the sender's most-recently-used position.
        self.directory.lock().await.touch(&from);

        // The receiver's durable inbox QUEUE, resolved from the target's *real*
        // addressable home (live or archive) so the durable record lands where the
        // restore drains it — an archived App lives under the archive, and writing
        // to a fresh live directory would shadow its manifest.
        let app_dir = self.store.addressable_home(&to.app_id);
        let mailbox = crate::app::peer::mailbox::Mailbox::in_dir(&app_dir);
        // The durable envelope (id + payload + auth) — present when the payload is a
        // `DurableEnvelope` (the durable peer path). A legacy/plain payload carries
        // no envelope and is delivered best-effort (undeduped) on the path below.
        let durable_envelope = serde_json::from_value::<DurableEnvelope>(payload.clone()).ok();

        match durable_envelope {
            // DURABLE PATH: fsync-before-deliver + Delivered-after-durable-fold.
            Some(durable) => {
                self.relay_durable(from, to, payload, durable, &mailbox)
                    .await
            }
            // BEST-EFFORT PATH (legacy/plain payload, no envelope id): no durable
            // dedup key, so deliver to a live sink or queue to the inbox as before.
            None => self.relay_plain(from, to, payload, &mailbox).await,
        }
    }

    /// The DURABLE relay (INV-3/INV-4): fsync the WHOLE envelope to the receiver's
    /// durable inbox queue BEFORE any delivery, then deliver to the live sink and
    /// report `Delivered` ONLY after the receiver confirms a DURABLE fold (its
    /// stratum-1 World log appended the peer input), ADVANCING the inbox past the
    /// envelope. If no durable confirmation arrives — the receiver is offline, its
    /// sink just closed, or the fold ack timed out — the outcome is `Queued` and the
    /// envelope PERSISTS in the durable inbox for the restore-drain to redeliver
    /// (the receiver's stratum-1 dedup makes a redelivery of an already-folded
    /// envelope a NO-OP). No crash window both dedups and drops: the SOLE dedup
    /// record is the receiver's fold, and the inbox is advanced only AFTER it.
    async fn relay_durable(
        &self,
        from: crate::app::peer::PeerId,
        to: crate::app::peer::PeerId,
        payload: serde_json::Value,
        durable: DurableEnvelope,
        mailbox: &crate::app::peer::mailbox::Mailbox,
    ) -> Result<PeerRouteOutcome, Error> {
        // RejectNewest peer-inbox cap (INV-5): if the durable inbox is at the cap
        // and this envelope is NOT already queued (a new, not a retried, send),
        // reject the incoming send — RejectNewest — without touching the inbox.
        // A retry of a still-queued envelope (same id) is not a new enqueue so it
        // passes through; `enqueue_unique` makes it a no-op below. `0` = unbounded
        // (skip the check). See docs/agent/world/ecs-runtime.md §1335–1338.
        if self.peer_inbox_cap > 0 {
            let queued = mailbox.peek()?;
            let already = queued
                .iter()
                .any(|e| e.durable().map(|d| d.id).as_ref() == Some(&durable.id));
            if !already && queued.len() >= self.peer_inbox_cap as usize {
                log::debug!(
                    "[peer-broker] peer inbox for {} full ({} >= cap {}); rejecting envelope {}",
                    to.app_id,
                    queued.len(),
                    self.peer_inbox_cap,
                    durable.id.0
                );
                return Ok(PeerRouteOutcome::Rejected);
            }
        }

        // fsync-before-deliver (INV-3): the envelope is durable in the inbox queue
        // BEFORE delivery, so a crash at any later point cannot lose it. It stays
        // queued until the receiver confirms a durable fold (advance, below). The
        // enqueue is IDEMPOTENT by envelope id so a retry (fsync-before-deliver runs
        // on every attempt) of a still-queued envelope does not pile up duplicates.
        let envelope = crate::app::peer::mailbox::PeerEnvelope {
            from: from.clone(),
            payload: payload.clone(),
        };
        mailbox.enqueue_unique(&envelope, &durable.id)?;

        let sink = self.sinks.lock().await.get(&to).cloned();
        if let Some(sink) = sink {
            let frame = crate::protocol::HostToAgent::PeerDeliver {
                // The protocol `from` names the sending App; the node identity lives
                // in the directory/envelope, not in this user-facing field.
                from: from.app_id.to_string(),
                payload,
            };
            if sink.send(frame).await.is_ok() {
                // Delivered to a live worker. INV-3: report `Delivered` ONLY after
                // the receiver confirms a DURABLE fold — NOT on this `sink.send` Ok
                // (an in-memory enqueue, not durability). On confirmation, ADVANCE
                // the durable inbox past this envelope (INV-4).
                if self.await_fold(&durable.id).await {
                    mailbox.advance(&durable.id)?;
                    self.directory.lock().await.touch(&to);
                    return Ok(PeerRouteOutcome::Delivered);
                }
                // No durable confirmation in time: the message is NOT lost — it is
                // still in the durable inbox. Report `Queued`; the restore-drain
                // redelivers it and the receiver's stratum-1 dedup absorbs any
                // already-applied redelivery.
                log::debug!(
                    "[peer-broker] envelope {} delivered to {} but no durable-fold ack; \
                     left queued for restore-drain (Queued, not lost)",
                    durable.id.0,
                    to.app_id
                );
                return Ok(PeerRouteOutcome::Queued { wake: to });
            }
            // The sink was closed out from under us (the worker is stopping): the
            // envelope is already durably queued, so just drop the stale sink and
            // report Queued.
            self.unregister(&to).await;
        }

        // The target is offline (tombstoned or archived) or its sink just closed:
        // the envelope is already in the durable, reconstructable inbox. Ask the
        // caller to wake the target so the restore-drain delivers it.
        log::debug!(
            "[peer-broker] envelope {} queued to {} inbox (durable; redelivered on restore)",
            durable.id.0,
            to.app_id
        );
        Ok(PeerRouteOutcome::Queued { wake: to })
    }

    /// The BEST-EFFORT relay for a legacy/plain payload that carries no durable
    /// envelope id (so there is no cross-process dedup key and no durable-fold ack
    /// to await): deliver to the live sink, or queue to the inbox if the target is
    /// offline. Preserves the pre-durable behavior for non-durable messages.
    async fn relay_plain(
        &self,
        from: crate::app::peer::PeerId,
        to: crate::app::peer::PeerId,
        payload: serde_json::Value,
        mailbox: &crate::app::peer::mailbox::Mailbox,
    ) -> Result<PeerRouteOutcome, Error> {
        let sink = self.sinks.lock().await.get(&to).cloned();
        if let Some(sink) = sink {
            let frame = crate::protocol::HostToAgent::PeerDeliver {
                from: from.app_id.to_string(),
                // Clone so the envelope survives for the inbox fall-through if the
                // sink turns out to be closed.
                payload: payload.clone(),
            };
            if sink.send(frame).await.is_ok() {
                self.directory.lock().await.touch(&to);
                return Ok(PeerRouteOutcome::Delivered);
            }
            self.unregister(&to).await;
        }

        // RejectNewest peer-inbox cap: a full inbox rejects the incoming send
        // without enqueueing anything. `0` = unbounded (skip the check). See
        // docs/agent/world/ecs-runtime.md §"Durable peer delivery" §1335–1338.
        if self.peer_inbox_cap > 0 && mailbox.depth()? >= self.peer_inbox_cap as usize {
            log::debug!(
                "[peer-broker] peer inbox for {} full (cap {}); rejecting plain send",
                to.app_id, self.peer_inbox_cap
            );
            return Ok(PeerRouteOutcome::Rejected);
        }
        let envelope = crate::app::peer::mailbox::PeerEnvelope { from, payload };
        mailbox.enqueue(&envelope)?;
        Ok(PeerRouteOutcome::Queued { wake: to })
    }
}

// ---------------------------------------------------------------------------
// BrokerPeerSender: the World↔broker adapter (the real PeerSender)
// ---------------------------------------------------------------------------

/// The real [`PeerBroker`]-backed [`PeerSender`]: the shell ADAPTER between the
/// World's outbound `Command::SendPeer` and the host's switch-only [`PeerBroker`].
/// When the live `WorldDriver` dispatches a peer drive it hands the destination, the
/// `payload`, and the sender-assigned `envelope` here; this wraps them in a
/// [`DurableEnvelope`] — so the RECEIVER reconstructs the `DriveRequested`/
/// `PeerDelivered` input verbatim (id + payload + auth) — relays it through
/// [`PeerBroker::relay`], and maps the broker's [`PeerRouteOutcome`] to the World's
/// [`DeliveryOutcome`] that the recorded `PeerSendOutcome` carries:
/// `Delivered → Delivered`, `Queued → Queued`, `Rejected → Rejected`. This is the
/// real sender injected on the (in-process) WorldDriver drive path via
/// [`submit_with_peer_sender`](crate::agent::world::driver::WorldDriver::submit_with_peer_sender),
/// REPLACING the inert `UnattachedPeerSender`. Durable ack / idempotency hardening is
/// a later pass — the broker is best-effort here. See docs/agent/world/ecs-runtime.md
/// (§645; Theme 4b; the World↔broker boundary).
//
// The production caller — an in-process host that drives two local WorldDrivers over
// one broker (the loopback path, PA-sink) — lands separately, so the bin sees this
// adapter as unused until then; the host-test below constructs it. Same not-yet-wired
// dead-code allowance the sibling resume-only driver helpers carry.
#[allow(dead_code)]
pub struct BrokerPeerSender {
    /// The switch the drive is relayed through.
    broker: Arc<PeerBroker>,
    /// This sender's own peer identity — the relay `from`, and (mirrored to a
    /// World-side `PeerId`) the `auth.from` the receiver authorizes the drive against.
    from: crate::app::peer::PeerId,
}

#[allow(dead_code)]
impl BrokerPeerSender {
    /// A sender that relays `from`'s peer drives through `broker`.
    pub fn new(broker: Arc<PeerBroker>, from: crate::app::peer::PeerId) -> Self {
        Self { broker, from }
    }

    /// The World-side mirror of this sender's identity, asserted as the drive's
    /// `auth.from` so the receiver's shape-level authorization (`auth.from == from`)
    /// passes (the receiver reconstructs `from` from the broker's frame on the same
    /// local node).
    fn world_from(&self) -> WorldPeerId {
        WorldPeerId {
            app_id: self.from.app_id.to_string(),
            node_id: self.from.node_id.to_string(),
        }
    }
}

impl PeerSender for BrokerPeerSender {
    async fn send(
        &self,
        to: &WorldPeerId,
        payload: &PeerPayload,
        _key: &CommandKey,
        envelope: &PeerEnvelopeId,
    ) -> Result<DeliveryOutcome, Error> {
        // The durable envelope carried over the wire so the receiver reconstructs the
        // inbound peer input verbatim (id + payload + auth).
        let durable = DurableEnvelope {
            id: envelope.clone(),
            payload: payload.clone(),
            auth: Authorization {
                from: self.world_from(),
                // Shape-level token (verification is a later enforcement pass): a
                // non-empty token whose `from` matches the sender authorizes the drive.
                token: self.from.app_id.to_string(),
            },
        };
        let wire = serde_json::to_value(&durable)?;
        let to_peer = crate::app::peer::PeerId::new(
            crate::app::AppId(to.app_id.clone()),
            crate::app::peer::NodeId(to.node_id.clone()),
        );
        // Map the broker's routing verdict to the recorded delivery outcome. The
        // `wake` target of a `Queued` is the supervisor's concern, not the sender's.
        let outcome = match self.broker.relay(self.from.clone(), to_peer, wire).await? {
            PeerRouteOutcome::Delivered => DeliveryOutcome::Delivered,
            PeerRouteOutcome::Queued { .. } => DeliveryOutcome::Queued,
            PeerRouteOutcome::Rejected => DeliveryOutcome::Rejected,
        };
        Ok(outcome)
    }
}

// ---------------------------------------------------------------------------
// Sender outbox reconstruction (PB-outbox): the still-Queued peer sends,
// rebuilt from the durable World log rather than held only in memory
// ---------------------------------------------------------------------------

/// The logical-send identity a `PeerSendOutcome` reconciles by when reconstructing
/// the sender's outbox: the destination plus the content-addressed request
/// `fingerprint`. A retry of the SAME logical send re-dispatches a fresh `SendPeer`
/// (a new `cmd`) but re-hashes to the SAME `fingerprint` for the SAME `to`, so the
/// outbox identity is CONTENT-addressed, NOT `cmd`-addressed (which would treat a
/// retry as a distinct send). The sender-assigned envelope id is itself derived
/// from this fingerprint (`effects.rs`: `PeerEnvelopeId(fingerprint.0)`), so keying
/// by `fingerprint` IS keying by `envelope`. Holds the fingerprint's inner `String`
/// rather than `Fingerprint` so the key is `Ord` for the canonical `BTreeMap`.
/// See docs/agent/world/ecs-runtime.md §"Durable peer delivery" (OUTBOX RECONSTRUCTION).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutboxKey {
    /// The destination peer of the send.
    pub to: WorldPeerId,
    /// The content-addressed request fingerprint (the envelope id's basis).
    pub fingerprint: String,
}

/// One outstanding entry in the reconstructed sender outbox: a logical peer send
/// whose LATEST recorded `PeerSendOutcome` is `Queued` (never superseded by a later
/// `Delivered`). Carries the destination so the restore-retry knows which peer's
/// durable inbox to re-drain. See docs/agent/world/ecs-runtime.md §"Durable peer
/// delivery" (OUTBOX RECONSTRUCTION).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// The originating `SendPeer` command of the latest attempt for this send.
    pub cmd: CmdId,
    /// The sender entity whose `Peer` slot the send settles.
    pub entity: EntityId,
    /// The destination peer whose durable inbox holds the still-queued envelope.
    pub to: WorldPeerId,
}

/// Reconstruct the sender's OUTBOX — the still-unacknowledged `Queued` peer sends —
/// PURELY from the durable World log, rather than holding it only in memory
/// (PB-outbox; VC-4.1). The outbox is `{Queued} − {later Delivered}`: folding the
/// `PeerSendOutcome` records in log order, the LATEST outcome per logical send
/// (keyed by `(to, fingerprint)`) wins, and the outbox keeps exactly those whose
/// latest outcome is still `Queued`. So a send that was `Queued` and later
/// `Delivered` (a retry that succeeded) is EXCLUDED, while a never-`Delivered`
/// `Queued` send REMAINS outstanding and is retried on peer restore — the receiver's
/// durable inbox redelivers it (PB-broker's `drain_peer_inbox`) and its stratum-1
/// dedup makes an already-applied one a no-op (effectively-once). A `Rejected`
/// outcome is terminal (the inbox was full) and is likewise not outstanding.
/// Deterministic and IO-free (`BTreeMap`, no `HashMap`/float) so it is unit-testable
/// over a recorded log. See docs/agent/world/ecs-runtime.md §"Durable peer delivery"
/// (OUTBOX RECONSTRUCTION).
pub fn reconstruct_outbox<'a>(
    inputs: impl IntoIterator<Item = &'a LogicalInput>,
) -> BTreeMap<OutboxKey, OutboxEntry> {
    // Fold the sender log: the LATEST PeerSendOutcome per logical send wins, so a
    // later Delivered/Rejected supersedes an earlier Queued for the same send.
    let mut latest: BTreeMap<OutboxKey, (DeliveryOutcome, OutboxEntry)> = BTreeMap::new();
    for input in inputs {
        if let LogicalInput::PeerSendOutcome {
            cmd,
            entity,
            fingerprint,
            to,
            outcome,
        } = input
        {
            let key = OutboxKey {
                to: to.clone(),
                fingerprint: fingerprint.0.clone(),
            };
            latest.insert(
                key,
                (
                    *outcome,
                    OutboxEntry {
                        cmd: *cmd,
                        entity: *entity,
                        to: to.clone(),
                    },
                ),
            );
        }
    }
    // The outbox is exactly the sends whose latest outcome is still Queued.
    latest
        .into_iter()
        .filter_map(|(key, (outcome, entry))| {
            (outcome == DeliveryOutcome::Queued).then_some((key, entry))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Surface router: per-App surface frame routing (VC-P.1)
// ---------------------------------------------------------------------------

/// Delivery sink for `HostToAgent::SurfaceDrive` frames to the macOS app
/// client. The router writes one frame per drive; the macOS client stream
/// drains it. See `docs/agent/world/ecs-runtime.md` (Theme 2a).
pub type SurfaceDriveSink = tokio::sync::mpsc::Sender<crate::protocol::HostToAgent>;

/// Inbound sink for `AgentToHost` surface frames from the macOS app client.
/// The router writes `SurfaceObservation`/`SurfaceMutated` frames here; the
/// App's ECS World or worker bridge drains them. See
/// `docs/agent/world/ecs-runtime.md` (Theme 2b; Inv 18).
pub type SurfaceInboundSink = tokio::sync::mpsc::Sender<crate::protocol::AgentToHost>;

/// Per-App surface routing table. Routes `HostToAgent::SurfaceDrive` from the
/// worker socket to the correct per-App macOS client stream, and routes inbound
/// `AgentToHost::{SurfaceObservation,SurfaceMutated}` from the client to the
/// correct App's worker seam — both keyed by App identity rather than
/// connection-accept order. Two Apps connecting in either order route correctly.
/// See `docs/agent/world/ecs-runtime.md` (Theme 2a/2b; VC-P.1).
pub struct SurfaceRouter {
    /// Per-App macOS client drive sinks. The host writes `SurfaceDrive` frames
    /// here; the macOS client stream delivers them to the native UI.
    drives: Mutex<HashMap<String, SurfaceDriveSink>>,
    /// Per-App worker inbound sinks. The macOS client writes
    /// `SurfaceObservation`/`SurfaceMutated` frames here; the worker bridge or
    /// ECS World folds them into `LogicalInput`s.
    inbound: Mutex<HashMap<String, SurfaceInboundSink>>,
}

impl SurfaceRouter {
    /// An empty router with no App registrations.
    pub fn new() -> Self {
        Self {
            drives: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
        }
    }

    /// Register the macOS app client's drive sink for `app_id`. Called when a
    /// client stream connects and identifies its App. Order-independent: two
    /// Apps may register in any order and route correctly.
    pub async fn register_client(&self, app_id: String, sink: SurfaceDriveSink) {
        self.drives.lock().await.insert(app_id, sink);
    }

    /// Register the App's worker inbound seam for `app_id`. Called when the
    /// worker's surface-observation channel is provisioned.
    pub async fn register_worker(&self, app_id: String, sink: SurfaceInboundSink) {
        self.inbound.lock().await.insert(app_id, sink);
    }

    /// Deregister the macOS client's drive sink for `app_id` (client disconnected).
    pub async fn unregister_client(&self, app_id: &str) {
        self.drives.lock().await.remove(app_id);
    }

    /// Deregister the worker's inbound seam for `app_id` (worker stopped).
    pub async fn unregister_worker(&self, app_id: &str) {
        self.inbound.lock().await.remove(app_id);
    }

    /// Route a `HostToAgent::SurfaceDrive` to `app_id`'s registered macOS
    /// client. Returns `true` if delivered. A missing or closed sink is logged
    /// and returns `false`; never blocks or propagates an error.
    pub async fn route_drive(
        &self,
        app_id: &str,
        frame: crate::protocol::HostToAgent,
    ) -> bool {
        let sink = self.drives.lock().await.get(app_id).cloned();
        if let Some(sink) = sink {
            match sink.try_send(frame) {
                Ok(()) => return true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    log::warn!(
                        "[surface-router] SurfaceDrive for App `{app_id}`: client sink full, frame dropped"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    log::warn!(
                        "[surface-router] SurfaceDrive for App `{app_id}`: client disconnected"
                    );
                    // Sink closed: client disconnected; remove the stale entry.
                    self.drives.lock().await.remove(app_id);
                }
            }
        } else {
            log::debug!(
                "[surface-router] SurfaceDrive for App `{app_id}`: no client registered"
            );
        }
        false
    }

    /// Route an inbound `AgentToHost` surface frame (`SurfaceObservation` or
    /// `SurfaceMutated`) from `app_id`'s macOS client to the App's worker seam.
    /// Returns `true` if delivered.
    pub async fn route_inbound(
        &self,
        app_id: &str,
        frame: crate::protocol::AgentToHost,
    ) -> bool {
        let sink = self.inbound.lock().await.get(app_id).cloned();
        if let Some(sink) = sink {
            match sink.try_send(frame) {
                Ok(()) => return true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    log::warn!(
                        "[surface-router] surface inbound for App `{app_id}`: worker seam full, frame dropped"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    log::warn!(
                        "[surface-router] surface inbound for App `{app_id}`: worker seam closed"
                    );
                    // Sink closed: worker stopped; remove the stale entry.
                    self.inbound.lock().await.remove(app_id);
                }
            }
        } else {
            log::debug!(
                "[surface-router] surface inbound for App `{app_id}`: no worker seam registered"
            );
        }
        false
    }
}

/// Run the surface-client accept loop: the macOS GUI client connects here so the
/// host's [`SurfaceRouter`] can relay surface frames between it and the App's
/// worker subprocess. Each accepted connection is handled on its own task; an
/// accept error is logged and the loop continues. Runs for the host's lifetime.
///
/// This is a dedicated raw-TCP length-prefixed-frame listener (the same framing
/// as the worker RPC link), following the `accept_worker` pattern. It is NOT the
/// gateway HTTP port (19385) and NOT the worker RPC port (19384, which routes
/// worker `Hello`s). See `docs/agent/world/ecs-runtime.md`.
pub async fn run_surface_client_listener(listener: TcpListener, router: Arc<SurfaceRouter>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_surface_client(stream, router).await {
                        log::warn!("[surface-client] connection from {addr} ended: {e}");
                    }
                });
            }
            Err(e) => log::error!("[surface-client] accept error: {e}"),
        }
    }
}

/// Handle one macOS surface client: perform the registration handshake, register
/// its drive sink with the router, then pump the client's inbound surface frames
/// to the App's worker.
///
/// REGISTRATION HANDSHAKE (the contract W5's macOS client targets): the client's
/// FIRST frame MUST be [`AgentToHost::Hello`] carrying the App id it observes.
/// The host then:
/// - registers a drive sink with [`SurfaceRouter::register_client`] — a spawned
///   task drains [`HostToAgent::SurfaceDrive`] frames the router routes and writes
///   each to the client socket (the host→client drive leg), and
/// - reads the client's [`AgentToHost::SurfaceObservation`]/[`SurfaceMutated`]
///   frames and routes each to the App's worker via
///   [`SurfaceRouter::route_inbound`] (the client→worker observation leg).
///
/// A non-`Hello` first frame is rejected; a clean EOF before registering is a
/// no-op. On disconnect the client's drive sink is unregistered.
async fn handle_surface_client(stream: TcpStream, router: Arc<SurfaceRouter>) -> Result<(), Error> {
    let (mut reader, mut writer) = stream.into_split();

    // The registration frame must be first on the wire so the host can route this
    // client to the right App without relying on accept order.
    let app_id = match protocol::read_message::<AgentToHost>(&mut reader).await? {
        Some(AgentToHost::Hello { app_id }) => app_id,
        Some(other) => {
            return Err(Error::Rpc(format!(
                "surface client's first frame was not a Hello registration: {other:?}"
            )));
        }
        None => return Ok(()),
    };
    log::info!("[surface-client] registered for App `{app_id}`");

    // Register the client's drive sink: a task drains the routed drive frames and
    // writes each to the client socket. The router holds only the sender, keeping
    // the relay stateless-per-frame.
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel::<HostToAgent>(SURFACE_CLIENT_DRIVE_CAPACITY);
    router.register_client(app_id.clone(), drive_tx).await;

    let drive_app_id = app_id.clone();
    let drive_task = tokio::spawn(async move {
        while let Some(frame) = drive_rx.recv().await {
            if let Err(e) = protocol::write_message(&mut writer, &frame).await {
                log::warn!("[surface-client:{drive_app_id}] failed to write drive: {e}");
                break;
            }
        }
    });

    // Inbound pump: route the client's observed/mutated surface frames to the
    // App's worker. Other frames are ignored — only surface observations cross
    // this seam.
    let result = async {
        loop {
            match protocol::read_message::<AgentToHost>(&mut reader).await? {
                Some(frame @ AgentToHost::SurfaceObservation { .. })
                | Some(frame @ AgentToHost::SurfaceMutated { .. }) => {
                    router.route_inbound(&app_id, frame).await;
                }
                Some(other) => log::debug!(
                    "[surface-client:{app_id}] ignoring non-surface frame: {other:?}"
                ),
                None => {
                    log::info!("[surface-client:{app_id}] disconnected");
                    break;
                }
            }
        }
        Ok::<(), Error>(())
    }
    .await;

    router.unregister_client(&app_id).await;
    drive_task.abort();
    result
}

/// The read/write halves of an accepted worker socket, paired so a caller can
/// keep streaming after the routing decision has been made.
pub struct WorkerStream {
    pub reader: tokio::net::tcp::OwnedReadHalf,
    pub writer: tokio::net::tcp::OwnedWriteHalf,
}

/// The outcome of inspecting an accepted worker socket's first frame.
///
/// Incoming worker sockets are routed by their first frame rather than by
/// accept order: a native app worker opens with
/// [`AgentToHost::Hello`](crate::protocol::AgentToHost::Hello), while a VM child
/// opens with a task frame ([`AgentToHost::Response`] /
/// [`AgentToHost::ExternalInteraction`]). See
/// `docs/app/runtime/worker-lifecycle.md`.
pub enum AcceptedWorker {
    /// A native app worker identified by its `Hello` frame.
    App {
        app_id: String,
        stream: WorkerStream,
    },
    /// A VM child connection. `first_frame` is the frame already read off the
    /// wire while classifying the socket, handed back so the VM path processes
    /// it exactly as before.
    VmChild {
        first_frame: AgentToHost,
        stream: WorkerStream,
    },
}

/// Accept one worker connection and classify it by its first frame.
///
/// This replaces accept-by-order routing: the first frame decides whether the
/// socket belongs to a native app worker (`Hello`) or a VM child (a task frame).
/// The VM child's first frame is returned so no message is lost.
pub async fn accept_worker(listener: &TcpListener) -> Result<AcceptedWorker, Error> {
    let (stream, addr) = listener.accept().await?;
    let (mut reader, writer) = stream.into_split();
    match protocol::read_message::<AgentToHost>(&mut reader).await? {
        Some(AgentToHost::Hello { app_id }) => {
            log::info!("App worker {} connected from {}", app_id, addr);
            Ok(AcceptedWorker::App {
                app_id,
                stream: WorkerStream { reader, writer },
            })
        }
        Some(first_frame) => {
            log::info!("VM child connected from {}", addr);
            Ok(AcceptedWorker::VmChild {
                first_frame,
                stream: WorkerStream { reader, writer },
            })
        }
        None => Err(Error::Rpc(format!(
            "worker at {addr} disconnected before sending any frame"
        ))),
    }
}

/// Configuration for host mode.
#[derive(Clone)]
pub struct HostConfig {
    pub vm_image: String,
    pub share_root: PathBuf,
    pub rpc_port: u16,
    /// TCP port the macOS GUI surface client connects to. See
    /// [`DEFAULT_SURFACE_PORT`] and `docs/agent/world/ecs-runtime.md`.
    pub surface_port: u16,
    pub host_ip: String,
    pub agent_binary_path: Option<String>,
    pub agent_env: HashMap<String, String>,
    pub agent_data_dir: Option<PathBuf>,
    pub memory_mb: Option<usize>,
    pub cpu_count: Option<usize>,
}

impl HostConfig {
    pub fn from_env() -> Self {
        let image = std::env::var("RUBBERDUX_VM_IMAGE")
            .ok()
            .map(|raw| {
                crate::vm::setup::get_image(&raw)
                    .map(|img| img.base_vm_name.to_string())
                    .unwrap_or(raw)
            })
            .unwrap_or_else(|| "rubberdux-base-ubuntu24-release".to_string());

        let share_root = std::env::var("RUBBERDUX_VM_SHARES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./vm-shares"));

        let rpc_port: u16 = std::env::var("RUBBERDUX_RPC_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RPC_PORT);

        let surface_port: u16 = std::env::var("RUBBERDUX_SURFACE_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SURFACE_PORT);

        let host_ip =
            std::env::var("RUBBERDUX_HOST_IP").unwrap_or_else(|_| "192.168.64.1".to_string());

        let agent_data_dir = std::env::var("RUBBERDUX_AGENT_DATA_DIR")
            .map(PathBuf::from)
            .ok();

        // Propagate LLM configuration to the agent VM
        let mut agent_env = HashMap::new();
        for key in [
            "RUBBERDUX_LLM_BASE_URL",
            "RUBBERDUX_LLM_API_KEY",
            "RUBBERDUX_LLM_MODEL",
            "RUBBERDUX_LLM_USER_AGENT",
        ] {
            if let Ok(value) = std::env::var(key) {
                agent_env.insert(key.to_string(), value);
            }
        }

        Self {
            vm_image: image,
            share_root,
            rpc_port,
            surface_port,
            host_ip,
            agent_binary_path: None,
            agent_env,
            agent_data_dir,
            memory_mb: None,
            cpu_count: None,
        }
    }
}

fn build_agent_command(config: &HostConfig, task_id: Option<&str>) -> String {
    let binary = config.agent_binary_path.as_deref().unwrap_or("rubberduxd");
    let binary_quoted = shell_quote(binary);
    let mut cmd = format!(
        "{} --agent --rpc-host {}:{}",
        binary_quoted, config.host_ip, config.rpc_port
    );
    if let Some(tid) = task_id {
        cmd.push_str(&format!(" --task-id {}", shell_quote(tid)));
    }

    // Ensure the binary is executable and strip quarantine attributes (macOS)
    let mut setup = if config.agent_binary_path.is_some() {
        format!(
            "chmod +x {} && xattr -d com.apple.quarantine {} 2>/dev/null || true && ",
            binary_quoted, binary_quoted
        )
    } else {
        String::new()
    };

    // Set up persistent data directory symlinks inside the VM
    if config.agent_data_dir.is_some() {
        setup.push_str(
            "OS=\"$(uname -s)\"; \
            if [[ \"$OS\" == \"Darwin\" ]]; then \
                mkdir -p \"/Volumes/My Shared Files/data/\"{documents,downloads,config,sessions,tool-results,subagents}; \
                ln -sf \"/Volumes/My Shared Files/data/documents\" ~/Documents; \
                ln -sf \"/Volumes/My Shared Files/data/downloads\" ~/Downloads; \
                ln -sf \"/Volumes/My Shared Files/data/config\" ~/.rubberdux; \
                export RUBBERDUX_DATA_DIR=\"/Volumes/My Shared Files/data\"; \
            elif [[ \"$OS\" == \"Linux\" ]]; then \
                sudo mkdir -p /mnt/shared; \
                sudo mount -t virtiofs com.apple.virtio-fs.automount /mnt/shared 2>/dev/null || true; \
                mkdir -p /mnt/shared/data/{documents,downloads,config,sessions,tool-results,subagents}; \
                ln -sf /mnt/shared/data/documents ~/Documents; \
                ln -sf /mnt/shared/data/downloads ~/Downloads; \
                ln -sf /mnt/shared/data/config ~/.rubberdux; \
                export RUBBERDUX_DATA_DIR=\"/mnt/shared/data\"; \
            fi && "
        );
    }

    let cmd = setup + &cmd;

    if config.agent_env.is_empty() {
        format!("nohup {} > /tmp/rubberdux-agent.log 2>&1 &", cmd)
    } else {
        let exports: Vec<String> = config
            .agent_env
            .iter()
            .map(|(k, v)| format!("export {}={}", shell_quote(k), shell_quote(v)))
            .collect();
        let script = exports.join(" && ") + " && " + &cmd;
        format!(
            "nohup bash -c {} > /tmp/rubberdux-agent.log 2>&1 &",
            shell_quote(&script)
        )
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Run rubberdux in host mode.
///
/// The host runs the AgentLoop locally and bridges Telegram ↔ AgentLoop
/// via the broadcast-based adapter.
pub async fn run(config: HostConfig, bot: Option<Bot>) {
    use crate::agent::builder::AgentLoopBuilder;

    let rpc_listener = match TcpListener::bind(("0.0.0.0", config.rpc_port)).await {
        Ok(l) => {
            log::info!("RPC listener bound to 0.0.0.0:{}", config.rpc_port);
            Arc::new(l)
        }
        Err(e) => {
            log::warn!("Failed to bind RPC listener on port {}: {} (VM isolation disabled)", config.rpc_port, e);
            // Continue without VM support — isolate=true will return an error.
            // If even the loopback fallback cannot bind, the host has no RPC
            // transport at all: log and abort startup rather than panicking.
            match TcpListener::bind("127.0.0.1:0").await {
                Ok(l) => Arc::new(l),
                Err(e) => {
                    log::error!("Failed to bind fallback RPC listener: {e}; aborting host startup");
                    return;
                }
            }
        }
    };

    // The surface router relays surface frames between each App's worker
    // subprocess and the registered macOS GUI client, keyed by App id. The same
    // router is shared with the surface-client listener (which registers clients
    // and routes their observations inbound) and the App supervisor (whose per-App
    // pump registers the worker and routes its drives outbound). See
    // `docs/agent/world/ecs-runtime.md`.
    let surface_router = Arc::new(SurfaceRouter::new());

    // Stand up the surface-client listener: the dedicated host endpoint the macOS
    // GUI client (W5) connects to. A bind failure disables the live surface relay
    // but leaves the rest of the host serving.
    match TcpListener::bind(("0.0.0.0", config.surface_port)).await {
        Ok(listener) => {
            log::info!("Surface-client listener bound to 0.0.0.0:{}", config.surface_port);
            tokio::spawn(run_surface_client_listener(listener, surface_router.clone()));
        }
        Err(e) => log::warn!(
            "Failed to bind surface-client listener on port {}: {e} (live surface relay disabled)",
            config.surface_port
        ),
    }

    let mut vm_manager = VMManager::new(config.vm_image.clone(), config.share_root.clone());
    if let Some(mem) = config.memory_mb {
        vm_manager = vm_manager.with_memory_mb(mem);
    }
    if let Some(cpus) = config.cpu_count {
        vm_manager = vm_manager.with_cpu_count(cpus);
    }
    let vm_manager = Arc::new(Mutex::new(vm_manager));
    let host_config = Arc::new(config);

    // Initialize session manager and create new session
    let session_manager = Arc::new(crate::session::SessionManager::new());
    let model = std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());
    let (session_id, session_dir) = match session_manager.create_session(model) {
        Ok(session) => session,
        Err(e) => {
            log::error!("Failed to create session: {e}; aborting host startup");
            return;
        }
    };

    log::info!(
        "Created session: {} at {}",
        session_id.to_string(),
        session_dir.display()
    );

    // Create project root symlink if missing
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !project_root.join("sessions").exists() {
        if let Err(e) = crate::session::SessionManager::create_project_symlink(&project_root) {
            log::warn!("Failed to create project sessions symlink: {}", e);
        }
    }

    let mindset = Arc::new(crate::mindset::Mindset::new());
    if let Err(e) = mindset.ensure_dirs() {
        log::error!("Failed to initialize mindset: {e}; aborting host startup");
        return;
    }
    mindset.seed_defaults_if_empty(&project_root.join("prompts"));
    log::info!("Mindset root: {}", mindset.root.display());

    let workspace = Arc::new(crate::workspace::Workspace::new());
    if let Err(e) = workspace.ensure_dirs() {
        log::error!("Failed to initialize workspace: {e}; aborting host startup");
        return;
    }

    log::info!("Workspace root: {}", workspace.root.display());

    let interaction_queue = std::sync::Arc::new(
        crate::agent::external::interaction_queue::InteractionQueue::new(),
    );

    let mut conventions = crate::guardrail::ConventionRegistry::new();
    conventions.register(crate::channel::adapter::telegram_guardrails::convention());
    conventions.register(crate::workspace::convention());
    conventions.register(crate::mindset::convention());
    conventions.register(crate::agent::external::convention(interaction_queue.clone()));

    let convention_guidance = conventions.compose_guidance();
    let guardrails = conventions.build_guardrail_chain();

    let mut prompt_parts = crate::hardened_prompts::load_prompt_parts(&mindset.root);
    prompt_parts.push(convention_guidance);
    let channel_partial = Some(crate::channel::adapter::telegram::channel_prompt());
    let system_prompt =
        crate::hardened_prompts::compose_system_prompt(&prompt_parts, channel_partial);

    // Select the model provider from configuration; every turn (host chat and
    // each App worker) is driven through this one `ModelApi`. A bad/unknown
    // provider is a fatal startup error, handled like the other fatal init
    // failures above. `select_from_env` returns both the resolved selection
    // metadata (provider id, dialect, model) and the built adapter so only ONE
    // environment resolution is performed — the one-provider invariant.
    let (resolved_selection, client_box) = match crate::provider::select_from_env() {
        Ok(pair) => pair,
        Err(e) => {
            log::error!("Failed to select model provider: {e}; aborting host startup");
            return;
        }
    };
    let client: Arc<dyn crate::provider::ModelApi> = Arc::from(client_box);
    let provider_meta = crate::gateway::state::ProviderMeta {
        provider: resolved_selection.provider.as_str().to_string(),
        model: resolved_selection.model.clone(),
        dialect: resolved_selection.dialect.as_str().to_string(),
    };

    // Create the trajectory broadcast channel and recorder before building the
    // agent loop so that events flow to both the filesystem log and any
    // WebSocket subscribers on `/api/v1/ws/trajectory`.
    let (trajectory_tx, _) = tokio::sync::broadcast::channel(256);
    let events_path = session_manager
        .main_agent_dir(&session_id)
        .join("events.jsonl");
    let gateway_events_path = events_path.clone();
    let fs_recorder = crate::trajectory::filesystem_recorder(events_path);
    let broadcast_recorder: crate::trajectory::SharedTrajectoryRecorder = Arc::new(
        crate::trajectory::BroadcastTrajectoryRecorder::new(
            fs_recorder,
            trajectory_tx.clone(),
        ),
    );

    let gateway_system_prompt = system_prompt.clone();
    let telegram_chat_id: std::sync::Arc<tokio::sync::Mutex<Option<i64>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let mut builder = AgentLoopBuilder::new(system_prompt, session_manager)
        .with_session_id(session_id)
        .with_workspace(workspace)
        .with_mindset(mindset.clone())
        .with_guardrails(guardrails)
        .with_recorder(broadcast_recorder)
        .with_external_cwd(project_root.clone())
        .with_interaction_queue(interaction_queue.clone())
        .with_vm_infrastructure(vm_manager, rpc_listener, host_config);
    // Register the Telegram channel processor only when a bot is configured; the
    // gateway and app-board channels function without it.
    if let Some(bot) = bot.as_ref() {
        let telegram_processor = std::sync::Arc::new(
            crate::channel::adapter::telegram::TelegramChannelProcessor::new(
                bot.clone(),
                telegram_chat_id.clone(),
            )
            .with_interaction_queue(interaction_queue.clone()),
        );
        builder = builder.with_channel_processor("telegram", telegram_processor);
    }
    let (agent_loop, input_port, _context_tx) = builder.build(client.clone()).await;

    // Subscribe to entry broadcasts for the Telegram adapter
    let entry_rx = agent_loop.subscribe_output().into_receiver();

    // Bring up the multi-App board subsystem additively: the default
    // subprocess-backed `AppSupervisor` (`LocalSupervisor`) owns the
    // `Hello`-routing accept loop, the peer broker, and the idle sweeper, all set
    // up by `bind`. Apps load `Tombstoned` from the filesystem store at startup;
    // no worker spawns until an App is used. The supervisor is `None` when binding
    // fails, in which case the board surface is simply absent while the rest of
    // the host comes up unaffected. See `docs/app/runtime/worker-lifecycle.md`.
    let app_store: Arc<dyn crate::app::registry::store::AppStore> =
        Arc::new(crate::app::registry::store::FilesystemAppStore::new());
    // `bind_shared` returns the supervisor already in an `Arc` so each App's
    // supervision pump holds a live `Weak<Self>` and can wake an offline target
    // through `ensure_active` when a `PeerSend` is queued to its inbox. See
    // `docs/app/peer/decentralized-messaging.md`.
    let app_supervisor =
        match crate::app::runtime::local_supervisor::LocalSupervisor::bind_shared(
            app_store.clone(),
            surface_router.clone(),
        )
        .await
        {
            Ok(supervisor) => {
                match app_store.list(false) {
                    Ok(apps) => log::info!(
                        "App board ready: {} App(s) loaded Tombstoned at startup",
                        apps.len()
                    ),
                    Err(e) => log::warn!("Failed to enumerate Apps at startup: {e}"),
                }
                Some(supervisor)
            }
            Err(e) => {
                log::warn!("Failed to bind App supervisor: {e}; board surface disabled");
                None
            }
        };

    // Set up the gateway server
    let _gateway_handle = {
        let output_port = agent_loop.subscribe_output();
        let identity = std::fs::read_to_string(mindset.identity_path()).unwrap_or_default();
        let soul = std::fs::read_to_string(mindset.soul_path()).unwrap_or_default();
        let mut gateway_state = crate::gateway::state::GatewayState::with_trajectory_tx(
            gateway_system_prompt, identity, soul, trajectory_tx, input_port.clone(),
            Some(gateway_events_path),
            client.clone(),
            provider_meta,
        );
        // Light up the board REST + WS routes against the supervisor while keeping
        // the single-agent surface above. The identity client derives an App's
        // title + icon as a background task on creation. See `docs/gateway/apps.md`.
        if let Some(supervisor) = app_supervisor {
            // Identity derivation and clustering share the one selected provider
            // chosen above for the chat loop; no second selection is needed.
            gateway_state.attach_apps(supervisor, client.clone());
        }
        let gateway_state = Arc::new(gateway_state);
        let state_clone = gateway_state.clone();
        tokio::spawn(crate::gateway::stream::mirror_entries(output_port, state_clone));
        tokio::spawn(crate::gateway::server::run(gateway_state))
    };

    // Spawn AgentLoop
    tokio::spawn(async move {
        agent_loop.run().await;
    });

    match bot {
        // Run the Telegram adapter (blocks until the dispatcher shuts down).
        Some(bot) => {
            crate::channel::adapter::telegram::run(
                bot,
                input_port,
                entry_rx,
                telegram_chat_id,
                interaction_queue,
            )
            .await;
        }
        // No Telegram bridge: keep the host alive so the gateway, app board, and
        // agent loop keep serving until the process is interrupted.
        None => {
            log::info!("Host ready (Telegram bridge disabled). Press Ctrl-C to stop.");
            if let Err(e) = tokio::signal::ctrl_c().await {
                log::error!("Failed to listen for shutdown signal: {e}");
            }
        }
    }

    log::info!("Host shutdown complete.");
}

/// Run a child VM to completion and return the final output.
/// Guarantees the child VM is destroyed even if the agent fails or errors occur.
pub async fn run_child_vm(
    manager: Arc<Mutex<VMManager>>,
    task_id: &str,
    prompt: &str,
    subagent_type: &str,
    agent_name: Option<&str>,
    config: &HostConfig,
    listener: Arc<TcpListener>,
    interaction_queue: std::sync::Arc<crate::agent::external::interaction_queue::InteractionQueue>,
) -> Result<String, Error> {
    // Helper to write status updates to the child share for debugging
    async fn write_status(share_dir: &std::path::Path, msg: &str) {
        let _ = tokio::fs::write(share_dir.join("status.txt"), msg).await;
    }

    // Create and start child VM
    {
        let mut mgr = manager.lock().await;
        write_status(&mgr.share_dir(task_id), "run_child_vm: creating VM").await;
        mgr.create_and_start(task_id, None, config.agent_data_dir.as_deref())
            .await?;
    }

    // Run the child VM lifecycle with guaranteed cleanup
    let result = async {
        // Wait for SSH
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: waiting for SSH").await;
            mgr.wait_for_ssh(task_id).await?;
        }

        // Copy the agent binary from the main VM share to the child VM share
        // so the child can execute it.
        {
            let mgr = manager.lock().await;
            write_status(
                &mgr.share_dir(task_id),
                "run_child_vm: copying binary and prompt",
            )
            .await;
            let main_binary = config.share_root.join("main").join("rubberduxd");
            let child_binary = mgr.share_dir(task_id).join("rubberduxd");
            if main_binary.exists() {
                tokio::fs::copy(&main_binary, &child_binary).await?;
            }
            let prompt_path = mgr.share_dir(task_id).join("prompt.txt");
            tokio::fs::write(&prompt_path, prompt).await?;
            let subagent_type_path = mgr.share_dir(task_id).join("subagent_type.txt");
            tokio::fs::write(&subagent_type_path, subagent_type).await?;
            if let Some(name) = agent_name {
                let agent_name_path = mgr.share_dir(task_id).join("agent_name.txt");
                tokio::fs::write(&agent_name_path, name).await.map_err(|e| {
                    Error::Vm(format!("Failed to write agent_name.txt: {}", e))
                })?;
                log::info!("Wrote agent_name.txt: {}", name);
            }
        }

        // Start the agent inside the child VM
        let agent_cmd = build_agent_command(config, Some(task_id));
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: starting agent").await;
            let result = mgr.exec(task_id, &agent_cmd).await?;
            if result.exit_code != 0 {
                let err = format!(
                    "Child VM agent failed to start (exit {}): stdout={} stderr={}",
                    result.exit_code, result.stdout, result.stderr
                );
                write_status(&mgr.share_dir(task_id), &err).await;
                return Err(Error::Vm(err));
            }
        }

        // Copy child VM agent log to share immediately so it survives even if
        // listener.accept() hangs (helps debugging connection issues).
        {
            let mgr = manager.lock().await;
            let log_result = mgr
                .exec(task_id, "cat /tmp/rubberdux-agent.log 2>/dev/null || true")
                .await;
            let early_log = log_result.map(|r| r.stdout).unwrap_or_default();
            let log_path = mgr.share_dir(task_id).join("agent.log");
            let _ = tokio::fs::write(&log_path, &early_log).await;
            write_status(
                &mgr.share_dir(task_id),
                "run_child_vm: waiting for RPC connection",
            )
            .await;
        }

        // Accept the child's RPC connection, routing by the first frame instead
        // of by accept order. A native app worker would open with `Hello`; this
        // VM path only consumes VM-child sockets and hands any stray app socket
        // back to the listener for the supervisor (wired in a later task) by
        // closing it — no app worker is expected on this path.
        // See docs/app/runtime/worker-lifecycle.md.
        let (mut reader, writer, first_frame) = loop {
            match accept_worker(&listener).await? {
                AcceptedWorker::VmChild { first_frame, stream } => {
                    log::info!("Child VM {} matched a VM-child socket", task_id);
                    break (stream.reader, stream.writer, first_frame);
                }
                AcceptedWorker::App { app_id, stream } => {
                    log::warn!(
                        "App worker {} connected on VM-child accept path; closing (no supervisor here)",
                        app_id
                    );
                    drop(stream);
                }
            }
        };
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

        // Process the first frame that was already read while classifying the
        // socket, then continue reading until the child's final response.
        let mut final_text = String::new();
        let mut pending = Some(first_frame);
        loop {
            let msg: Option<AgentToHost> = match pending.take() {
                Some(frame) => Some(frame),
                None => protocol::read_message(&mut reader).await?,
            };
            match msg {
                Some(AgentToHost::Response { text, is_final, .. }) => {
                    final_text = text;
                    if is_final {
                        break;
                    }
                }
                Some(AgentToHost::SpawnVM { .. }) => {
                    // Defensive guard: child VMs no longer have the agent tool,
                    // so this should never happen. Log and ignore.
                    log::warn!("Child VM {} requested nested spawn (ignoring)", task_id);
                }
                Some(AgentToHost::ExternalInteraction { task_id: _ext_task_id, request }) => {
                    let request_id = crate::agent::external::get_request_id(&request).to_string();
                    log::info!("VM child {} sent ExternalInteraction: {}", task_id, request_id);

                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    interaction_queue.add(
                        request_id.clone(),
                        crate::agent::external::interaction_queue::PendingInteraction {
                            request,
                            response_tx: resp_tx,
                        },
                    );

                    // Spawn task to write response back to VM when interaction is resolved
                    let writer_clone = writer.clone();
                    tokio::spawn(async move {
                        if let Ok(response) = resp_rx.await {
                            let msg = crate::protocol::HostToAgent::InteractionResponse {
                                request_id,
                                response,
                            };
                            let mut w = writer_clone.lock().await;
                            if let Err(e) = crate::protocol::write_message(&mut *w, &msg).await {
                                log::warn!("Failed to send InteractionResponse to VM: {}", e);
                            }
                        }
                    });
                }
                Some(other) => {
                    // App-worker frames (Hello/EntryNotification/peer/interaction)
                    // never arrive on the VM-child path — sockets are classified
                    // by their first frame in `accept_worker`. Log defensively.
                    log::warn!(
                        "Child VM {} sent unexpected frame on VM path: {:?}",
                        task_id,
                        other
                    );
                }
                None => {
                    log::info!("Child VM {} disconnected", task_id);
                    break;
                }
            }
        }

        // Copy child VM agent log to share for debugging before destruction
        {
            let mgr = manager.lock().await;
            let log_result = mgr
                .exec(task_id, "cat /tmp/rubberdux-agent.log 2>/dev/null || true")
                .await;
            let log_content = log_result.map(|r| r.stdout).unwrap_or_default();
            let log_path = mgr.share_dir(task_id).join("agent.log");
            let _ = tokio::fs::write(&log_path, &log_content).await;
            // Also persist on the host filesystem so it survives share cleanup
            let host_log_path =
                std::path::PathBuf::from(format!("/tmp/rubberdux-child-{}.log", task_id));
            let _ = tokio::fs::write(&host_log_path, &log_content).await;
        }

        Ok(final_text)
    }
    .await;

    // Destroy the child VM regardless of success or failure
    {
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.destroy(task_id).await {
            log::warn!("Failed to destroy child VM {}: {}", task_id, e);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct EnvVarGuard {
        key: &'static str,
        value: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, value }
        }

        fn set(key: &'static str, new_value: &str) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, new_value);
            }
            Self { key, value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.value {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[test]
    #[serial(host_config_env)]
    fn test_host_config_from_env_defaults() {
        let _guard = EnvVarGuard::unset("RUBBERDUX_RPC_PORT");
        // Test that HostConfig::from_env() doesn't panic when env vars are missing
        // by setting required vars if not present
        let config = HostConfig::from_env();
        assert_eq!(config.rpc_port, DEFAULT_RPC_PORT);
    }

    // --- PB-outbox: sender outbox reconstruction (VC-4.1) ---------------------

    use crate::agent::world::inputs::Fingerprint;

    fn peer_id(app: &str) -> WorldPeerId {
        WorldPeerId {
            app_id: app.into(),
            node_id: "local".into(),
        }
    }

    /// A sender-local `PeerSendOutcome` — the durable World-log record the outbox
    /// is reconstructed from. `entity` is fixed; the outbox identity is `(to, fp)`.
    fn peer_send_outcome(
        cmd: CmdId,
        to: &str,
        fp: &str,
        outcome: DeliveryOutcome,
    ) -> LogicalInput {
        LogicalInput::PeerSendOutcome {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(fp.into()),
            to: peer_id(to),
            outcome,
        }
    }

    /// VC-4.1: the reconstructed outbox is exactly `{Queued} − {later Delivered}`.
    /// A never-Delivered Queued send stays outstanding; a Queued-then-Delivered
    /// send is excluded; a Rejected send is terminal; non-send inputs are ignored.
    #[test]
    fn reconstruct_outbox_is_queued_minus_later_delivered() {
        let log = vec![
            LogicalInput::UserMessage {
                to: 0,
                text: "ignored".into(),
            },
            peer_send_outcome(1, "B", "fp-b", DeliveryOutcome::Queued),
            peer_send_outcome(2, "C", "fp-c", DeliveryOutcome::Queued),
            // C's send was later Delivered (a retry that succeeded) — it leaves the
            // outbox even though an earlier Queued was recorded for the SAME send.
            peer_send_outcome(3, "C", "fp-c", DeliveryOutcome::Delivered),
            peer_send_outcome(4, "D", "fp-d", DeliveryOutcome::Rejected),
        ];
        let outbox = reconstruct_outbox(log.iter());

        // Only B's never-Delivered Queued send remains outstanding.
        assert_eq!(outbox.len(), 1, "only the still-Queued send is outstanding");
        let key = OutboxKey {
            to: peer_id("B"),
            fingerprint: "fp-b".into(),
        };
        let entry = outbox.get(&key).expect("B's queued send is in the outbox");
        assert_eq!(entry.to, peer_id("B"));
        assert_eq!(entry.cmd, 1);

        // A Queued-then-Delivered envelope is EXCLUDED.
        assert!(
            !outbox.contains_key(&OutboxKey {
                to: peer_id("C"),
                fingerprint: "fp-c".into(),
            }),
            "a Queued-then-Delivered envelope is excluded from the reconstructed outbox"
        );
        // A Rejected send is terminal, not outstanding.
        assert!(!outbox.contains_key(&OutboxKey {
            to: peer_id("D"),
            fingerprint: "fp-d".into(),
        }));
    }

    /// The LATEST `PeerSendOutcome` per logical send (in log order) wins, so the
    /// reconciliation is a fold, not a first-write: a send whose latest recorded
    /// outcome is `Queued` is outstanding regardless of an earlier outcome.
    #[test]
    fn reconstruct_outbox_latest_outcome_per_send_wins() {
        let log = vec![
            peer_send_outcome(1, "B", "fp-b", DeliveryOutcome::Delivered),
            peer_send_outcome(2, "B", "fp-b", DeliveryOutcome::Queued),
        ];
        assert_eq!(
            reconstruct_outbox(log.iter()).len(),
            1,
            "the latest Queued outcome makes the send outstanding (latest-wins fold)"
        );
    }

    /// VC-4.1: retry-on-restore re-delivers the outstanding set, and an already-acked
    /// envelope is NOT re-applied. The outbox names the outstanding destination + its
    /// durable envelope id (reconstructed from the log, not memory); the matching
    /// envelope persists in the receiver's durable inbox; the restore-drain redelivers
    /// it; once the receiver folds it the drain advances past it, so a later restore
    /// finds nothing — effectively-once with PB-broker's dedup.
    #[test]
    fn retry_on_restore_redelivers_outstanding_and_dedups_already_acked() {
        use crate::app::AppId;
        use crate::app::peer::PeerId as AppPeerId;
        use crate::app::peer::mailbox::{Mailbox, PeerEnvelope};

        // The reconstructed outbox: B's still-Queued send (durable, not memory-only).
        let env_id = PeerEnvelopeId("fp-b".into());
        let sender_log = vec![peer_send_outcome(1, "B", "fp-b", DeliveryOutcome::Queued)];
        let outbox = reconstruct_outbox(sender_log.iter());
        assert_eq!(
            outbox.len(),
            1,
            "B's queued send is outstanding (reconstructed from the durable log)"
        );

        // The matching durable envelope persists in B's inbox queue (the broker
        // fsynced it before delivery) — what `drain_peer_inbox` redelivers on restore.
        let dir = tempfile::tempdir().expect("tempdir");
        let mailbox = Mailbox::in_dir(dir.path());
        let durable = DurableEnvelope {
            id: env_id.clone(),
            payload: PeerPayload::Message(serde_json::json!({ "text": "hi" })),
            auth: Authorization {
                from: peer_id("A"),
                token: "A".into(),
            },
        };
        let envelope = PeerEnvelope {
            from: AppPeerId::local(AppId("A".into())),
            payload: serde_json::to_value(&durable).expect("serialize durable envelope"),
        };
        mailbox
            .enqueue_unique(&envelope, &env_id)
            .expect("fsync the queued envelope");

        // RETRY ON RESTORE: the outstanding send's envelope is redeliverable — a peek
        // of B's durable inbox (what the restore-drain reads) still holds it.
        let queued = mailbox.peek().expect("peek B's inbox");
        assert_eq!(
            queued.len(),
            1,
            "the outstanding queued envelope is redeliverable on restore"
        );
        assert_eq!(queued[0].durable().map(|d| d.id), Some(env_id.clone()));

        // ALREADY-ACKED → NOT re-applied: once the receiver durably folds it, the
        // restore-drain advances past it (PB-broker INV-4), so a subsequent restore
        // finds nothing to redeliver — effectively-once.
        assert!(
            mailbox.advance(&env_id).expect("advance past the folded envelope"),
            "the folded envelope is advanced out of the durable inbox"
        );
        assert!(
            mailbox.peek().expect("re-peek B's inbox").is_empty(),
            "an already-acked envelope is not re-applied on a subsequent restore"
        );

        // And once the sender's log records the Delivered, the send leaves the
        // reconstructed outbox: the durable outbox and the receiver's fold agree.
        let mut acked_log = sender_log.clone();
        acked_log.push(peer_send_outcome(2, "B", "fp-b", DeliveryOutcome::Delivered));
        assert!(
            reconstruct_outbox(acked_log.iter()).is_empty(),
            "once Delivered, the send leaves the reconstructed outbox (no re-retry)"
        );
    }

    #[test]
    #[serial(host_config_env)]
    fn test_host_config_custom_rpc_port() {
        let _guard = EnvVarGuard::set("RUBBERDUX_RPC_PORT", "12345");
        let config = HostConfig::from_env();
        assert_eq!(config.rpc_port, 12345);
    }

    #[test]
    fn test_build_agent_command_basic() {
        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            surface_port: DEFAULT_SURFACE_PORT,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: HashMap::new(),
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, None);
        assert!(cmd.contains("rubberduxd"));
        assert!(cmd.contains("--agent"));
        assert!(cmd.contains("192.168.64.1:19384"));
    }

    #[test]
    fn test_build_agent_command_with_task_id() {
        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            surface_port: DEFAULT_SURFACE_PORT,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: HashMap::new(),
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, Some("task-123"));
        assert!(cmd.contains("--task-id"));
        assert!(cmd.contains("task-123"));
    }

    // -- PeerBroker: directory + relay (a switch, not an orchestrator) --------

    fn broker_store() -> Arc<dyn crate::app::registry::store::AppStore> {
        let dir = tempfile::tempdir().unwrap().into_path();
        Arc::new(crate::app::registry::store::FilesystemAppStore::with_apps_dir(dir))
    }

    fn peer(app: &str) -> crate::app::peer::PeerId {
        crate::app::peer::PeerId::local(crate::app::AppId(app.into()))
    }

    #[tokio::test]
    async fn relay_delivers_to_a_live_sink() {
        let broker = PeerBroker::new(broker_store());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        broker.register(peer("b"), tx).await;

        let outcome = broker
            .relay(peer("a"), peer("b"), serde_json::json!({"text": "ping"}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Delivered);

        match rx.recv().await.unwrap() {
            crate::protocol::HostToAgent::PeerDeliver { from, payload } => {
                assert_eq!(from, "a");
                assert_eq!(payload["text"], "ping");
            }
            other => panic!("expected PeerDeliver, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn relay_queues_to_inbox_when_target_offline() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        // No sink registered for `b`: it is offline (tombstoned or archived).
        let outcome = broker
            .relay(peer("a"), peer("b"), serde_json::json!({"text": "later"}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Queued { wake: peer("b") });

        // The envelope landed in b's inbox, ready to drain on restore.
        let app_dir = store.app_dir(&crate::app::AppId("b".into()));
        let queued = crate::app::peer::mailbox::Mailbox::in_dir(&app_dir)
            .drain()
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].from, peer("a"));
        assert_eq!(queued[0].payload["text"], "later");
    }

    #[tokio::test]
    async fn relay_rejects_self_addressing() {
        let broker = PeerBroker::new(broker_store());
        let outcome = broker
            .relay(peer("a"), peer("a"), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Rejected);
    }

    #[tokio::test]
    async fn list_for_excludes_self_and_is_mru_ordered() {
        let broker = PeerBroker::new(broker_store());
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel(1);
        let (tx_c, _rx_c) = tokio::sync::mpsc::channel(1);
        broker.register(peer("a"), tokio::sync::mpsc::channel(1).0).await;
        broker.register(peer("b"), tx_b).await;
        broker.register(peer("c"), tx_c).await;

        let listed = broker.list_for(&peer("a")).await;
        assert!(!listed.contains(&peer("a")), "must not list self");
        // c registered most recently among the others.
        assert_eq!(listed, vec![peer("c"), peer("b")]);
    }

    /// A World-side `PeerId` for an App on the local node — the `to` the
    /// `BrokerPeerSender` is handed and the `from` it asserts in `auth`.
    fn world_peer(app: &str) -> WorldPeerId {
        WorldPeerId {
            app_id: app.into(),
            node_id: crate::app::peer::NodeId::LOCAL.into(),
        }
    }

    /// [Verifies VC-3.2] `BrokerPeerSender` maps every `PeerRouteOutcome` to its
    /// `DeliveryOutcome` and relays the sender-assigned `DurableEnvelope` verbatim so
    /// the receiver reconstructs the inbound peer input. Delivered (live sink),
    /// Queued (offline target → inbox), Rejected (self-addressing) all map through.
    #[tokio::test]
    async fn broker_peer_sender_maps_route_outcomes() {
        // Delivered: a live sink receives the relayed DurableEnvelope. The receiver's
        // durable-fold confirmation is pre-recorded so the relay reports `Delivered`
        // only after a durable fold (INV-3) without waiting for a live worker.
        let broker = Arc::new(PeerBroker::new(broker_store()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        broker.register(peer("b"), tx).await;
        broker.confirm_fold(&PeerEnvelopeId("env-1".into())).await;
        let sender = BrokerPeerSender::new(broker.clone(), peer("a"));
        let delivered = sender
            .send(
                &world_peer("b"),
                &PeerPayload::Message(serde_json::json!({ "text": "hi" })),
                &CommandKey,
                &PeerEnvelopeId("env-1".into()),
            )
            .await
            .unwrap();
        assert_eq!(delivered, DeliveryOutcome::Delivered);
        // The relayed payload is the DurableEnvelope (so the receiver reconstructs it).
        match rx.recv().await.unwrap() {
            HostToAgent::PeerDeliver { from, payload } => {
                assert_eq!(from, "a", "the broker stamps the sender App id");
                let env: DurableEnvelope =
                    serde_json::from_value(payload).expect("payload is a DurableEnvelope");
                assert_eq!(env.id, PeerEnvelopeId("env-1".into()));
            }
            other => panic!("expected PeerDeliver, got {other:?}"),
        }

        // Queued: an offline target → Queued.
        let broker2 = Arc::new(PeerBroker::new(broker_store()));
        let sender2 = BrokerPeerSender::new(broker2, peer("a"));
        let queued = sender2
            .send(
                &world_peer("offline"),
                &PeerPayload::Message(serde_json::json!({})),
                &CommandKey,
                &PeerEnvelopeId("env-2".into()),
            )
            .await
            .unwrap();
        assert_eq!(queued, DeliveryOutcome::Queued);

        // Rejected: addressing itself → Rejected, nothing relayed.
        let broker3 = Arc::new(PeerBroker::new(broker_store()));
        let sender3 = BrokerPeerSender::new(broker3, peer("a"));
        let rejected = sender3
            .send(
                &world_peer("a"),
                &PeerPayload::Message(serde_json::json!({})),
                &CommandKey,
                &PeerEnvelopeId("env-3".into()),
            )
            .await
            .unwrap();
        assert_eq!(rejected, DeliveryOutcome::Rejected);
    }

    // -- Durable peer delivery: fsync, Delivered-after-ack, dedup (VC-4.1/4.2) --

    /// A `DurableEnvelope` wire payload carrying envelope id `id` — the durable
    /// peer path the broker dedups by. Mirrors what `BrokerPeerSender`/the worker
    /// put on the wire.
    fn durable_wire(id: &str) -> serde_json::Value {
        let durable = DurableEnvelope {
            id: PeerEnvelopeId(id.into()),
            payload: PeerPayload::Message(serde_json::json!({ "text": "x" })),
            auth: Authorization {
                from: WorldPeerId {
                    app_id: "a".into(),
                    node_id: crate::app::peer::NodeId::LOCAL.into(),
                },
                token: "a".into(),
            },
        };
        serde_json::to_value(&durable).expect("serialise durable envelope")
    }

    fn receiver_mailbox(
        store: &Arc<dyn crate::app::registry::store::AppStore>,
        app: &str,
    ) -> crate::app::peer::mailbox::Mailbox {
        let app_dir = store.app_dir(&crate::app::AppId(app.into()));
        crate::app::peer::mailbox::Mailbox::in_dir(&app_dir)
    }

    /// [Verifies VC-4.1] [acceptance (c)] DURABLE relay: the envelope is fsynced to
    /// the receiver's durable inbox queue BEFORE delivery, and `Delivered` is
    /// reported ONLY after the receiver confirms a DURABLE fold — never on the bare
    /// `sink.send` Ok. On confirmation the inbox is ADVANCED past the envelope.
    #[tokio::test]
    async fn relay_reports_delivered_only_after_durable_fold_then_advances() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        broker.register(peer("b"), tx).await;

        // The receiver confirms the durable fold (its stratum-1 World log appended
        // the peer input) — pre-recorded so the relay does not wait on a live worker.
        broker.confirm_fold(&PeerEnvelopeId("env-1".into())).await;
        let outcome = broker
            .relay(peer("a"), peer("b"), durable_wire("env-1"))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Delivered);

        // The frame reached the live sink, and the durable inbox was advanced past
        // the now-folded envelope (no leftover queue entry).
        assert!(rx.recv().await.is_some(), "the delivery reaches the sink");
        assert!(rx.try_recv().is_err());
        assert!(
            receiver_mailbox(&store, "b").is_empty(),
            "the durable inbox is advanced past a confirmed-folded envelope"
        );
    }

    /// [Verifies VC-4.1] [acceptance (c)] An undelivered send to an OFFLINE peer is
    /// `Queued` and the envelope PERSISTS in the durable, reconstructable inbox —
    /// NOT reported `Delivered` (no live worker, no durable fold). The restore-drain
    /// redelivers it; PB-outbox retries it on restore.
    #[tokio::test]
    async fn relay_queues_durably_to_offline_peer() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        // No sink for `b`: it is offline.
        let outcome = broker
            .relay(peer("a"), peer("b"), durable_wire("env-2"))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Queued { wake: peer("b") });

        // The envelope sits in the durable, reconstructable inbox queue.
        let queued = receiver_mailbox(&store, "b").peek().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].durable().map(|d| d.id),
            Some(PeerEnvelopeId("env-2".into())),
            "the un-acked envelope persists in the durable inbox"
        );
    }

    /// [Verifies VC-4.2] [acceptance (b): the CRASH-WINDOW] A durable send is fsynced
    /// to the inbox and DELIVERED to a live sink, but the delivery is interrupted
    /// before the receiver durably folds (no fold ack) → `Queued`, and the envelope
    /// PERSISTS in the durable inbox (never deduped-and-dropped). On re-drain (the
    /// retry after the simulated crash) the receiver confirms the fold → `Delivered`
    /// EXACTLY ONCE and the inbox is advanced. The SOLE dedup record is the
    /// receiver's fold, so no crash window both dedups and drops.
    #[tokio::test]
    async fn crash_window_before_fold_keeps_envelope_redeliverable_exactly_once() {
        let store = broker_store();
        // A short fold-ack timeout so the unconfirmed first attempt resolves to
        // `Queued` promptly (no production wait).
        let broker =
            PeerBroker::new(store.clone()).with_fold_ack_timeout(Duration::from_millis(50));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        broker.register(peer("b"), tx).await;

        // First attempt: delivered to the sink, but NO durable-fold ack arrives
        // (the receiver "crashed" before folding). The relay times out → Queued, and
        // the envelope is NOT removed from the durable inbox — never deduped-and-dropped.
        let first = broker
            .relay(peer("a"), peer("b"), durable_wire("env-9"))
            .await
            .unwrap();
        assert_eq!(first, PeerRouteOutcome::Queued { wake: peer("b") });
        assert!(rx.recv().await.is_some(), "the first attempt did deliver to the sink");
        let surviving = receiver_mailbox(&store, "b").peek().unwrap();
        assert_eq!(
            surviving.len(),
            1,
            "the un-folded envelope PERSISTS in the durable inbox (not dropped)"
        );

        // Re-drain after the crash: the receiver now confirms the durable fold. The
        // redelivery is reported `Delivered` and the inbox is advanced — exactly once.
        broker.confirm_fold(&PeerEnvelopeId("env-9".into())).await;
        let second = broker
            .relay(peer("a"), peer("b"), durable_wire("env-9"))
            .await
            .unwrap();
        assert_eq!(second, PeerRouteOutcome::Delivered, "the redelivery is delivered once");
        assert!(
            receiver_mailbox(&store, "b").is_empty(),
            "the durable inbox is advanced exactly once after the confirmed fold"
        );
        // The receiver's sink saw the redelivery too (at-least-once transport); the
        // receiver's stratum-1 World dedup — proved in `systems::peer` — makes that
        // second fold a NO-OP, so the message is APPLIED exactly once.
        assert!(rx.recv().await.is_some(), "the redelivery reaches the sink (at-least-once)");
    }

    /// [Verifies VC-4.2] [acceptance (d): drain-dedup] At the broker there is NO
    /// pre-apply dedup: a redelivery of an already-folded envelope is RE-SENT to the
    /// receiver (at-least-once). The dedup is the RECEIVER's job (its stratum-1 World
    /// log) — the broker never suppresses a redelivery, so a redelivery can never be
    /// deduped-and-dropped at the broker. (The receiver-side NO-OP is proved in
    /// `systems::peer::inbound_redelivered_envelope_is_not_refolded`.)
    #[tokio::test]
    async fn broker_redelivers_at_least_once_dedup_is_the_receivers_job() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        broker.register(peer("b"), tx).await;

        // Two confirmed deliveries of the SAME envelope id (a sender retry). Both are
        // re-sent to the receiver — the broker keeps NO permanent dedup ledger.
        broker.confirm_fold(&PeerEnvelopeId("env-3".into())).await;
        let first = broker
            .relay(peer("a"), peer("b"), durable_wire("env-3"))
            .await
            .unwrap();
        assert_eq!(first, PeerRouteOutcome::Delivered);
        broker.confirm_fold(&PeerEnvelopeId("env-3".into())).await;
        let second = broker
            .relay(peer("a"), peer("b"), durable_wire("env-3"))
            .await
            .unwrap();
        assert_eq!(second, PeerRouteOutcome::Delivered);

        // BOTH reached the receiver (at-least-once) — never suppressed at the broker.
        assert!(rx.recv().await.is_some(), "the first delivery reaches the sink");
        assert!(
            rx.recv().await.is_some(),
            "the redelivery ALSO reaches the receiver — the broker does not dedup"
        );
    }

    #[tokio::test]
    async fn unregister_keeps_app_addressable_via_inbox() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        broker.register(peer("b"), tx).await;
        broker.unregister(&peer("b")).await;

        // Gone from the directory, but still addressable: the message is queued.
        assert!(!broker.list_for(&peer("a")).await.contains(&peer("b")));
        let outcome = broker
            .relay(peer("a"), peer("b"), serde_json::json!({"text": "x"}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Queued { wake: peer("b") });
    }

    // -- PeerBroker: peer_inbox RejectNewest cap (VC-4.3) ----------------------

    /// [Verifies VC-4.3] (a) A durable relay to an offline peer is rejected (nothing
    /// enqueued) when the target's inbox is at the cap (RejectNewest). The inbox
    /// remains at its pre-rejection depth — no new entry is added.
    #[tokio::test]
    async fn relay_durable_rejects_when_inbox_at_cap() {
        let store = broker_store();
        // Cap = 1: a single queued envelope fills the inbox.
        let broker = PeerBroker::new(store.clone()).with_peer_inbox_cap(1);

        // First send: inbox is empty → enqueued (Queued).
        let first = broker
            .relay(peer("a"), peer("b"), durable_wire("env-cap-1"))
            .await
            .unwrap();
        assert_eq!(first, PeerRouteOutcome::Queued { wake: peer("b") });
        assert_eq!(
            receiver_mailbox(&store, "b").depth().unwrap(),
            1,
            "first send lands in the inbox"
        );

        // Second send: inbox is at cap → Rejected; nothing enqueued.
        let second = broker
            .relay(peer("a"), peer("b"), durable_wire("env-cap-2"))
            .await
            .unwrap();
        assert_eq!(second, PeerRouteOutcome::Rejected, "full inbox rejects the new send");
        assert_eq!(
            receiver_mailbox(&store, "b").depth().unwrap(),
            1,
            "inbox unchanged — nothing enqueued on Rejected"
        );
    }

    /// [Verifies VC-4.3] (a) A plain (best-effort) relay to an offline peer is
    /// rejected when the inbox is at the cap (RejectNewest) — nothing enqueued.
    #[tokio::test]
    async fn relay_plain_rejects_when_inbox_at_cap() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone()).with_peer_inbox_cap(1);

        // Fill the inbox with one plain send.
        let first = broker
            .relay(peer("a"), peer("b"), serde_json::json!({ "text": "first" }))
            .await
            .unwrap();
        assert_eq!(first, PeerRouteOutcome::Queued { wake: peer("b") });
        assert_eq!(receiver_mailbox(&store, "b").depth().unwrap(), 1);

        // Second plain send: inbox at cap → Rejected; inbox is unchanged.
        let second = broker
            .relay(peer("a"), peer("b"), serde_json::json!({ "text": "second" }))
            .await
            .unwrap();
        assert_eq!(second, PeerRouteOutcome::Rejected, "full inbox rejects the plain send");
        assert_eq!(
            receiver_mailbox(&store, "b").depth().unwrap(),
            1,
            "inbox unchanged — RejectNewest leaves existing entries intact"
        );
    }

    /// [Verifies VC-4.3] (b) `peer_inbox_cap = 0` is unbounded: the broker accepts
    /// any number of envelopes (here, beyond any positive bound) without rejecting.
    #[tokio::test]
    async fn relay_with_zero_cap_is_unbounded() {
        let store = broker_store();
        // 0 = unbounded (the default).
        let broker = PeerBroker::new(store.clone());
        assert_eq!(broker.peer_inbox_cap, 0, "default cap is 0 (unbounded)");

        // Send several plain messages to an offline peer — all are Queued, none Rejected.
        for i in 0..5u32 {
            let outcome = broker
                .relay(
                    peer("a"),
                    peer("b"),
                    serde_json::json!({ "seq": i }),
                )
                .await
                .unwrap();
            assert_eq!(
                outcome,
                PeerRouteOutcome::Queued { wake: peer("b") },
                "send {i} must be Queued (unbounded)"
            );
        }
        assert_eq!(
            receiver_mailbox(&store, "b").depth().unwrap(),
            5,
            "all 5 envelopes queued — 0 cap never rejects"
        );
    }

    /// [Verifies VC-4.3] A durable retry of an already-queued envelope passes through
    /// even when the inbox is at cap: the retry does NOT add a new entry (idempotent),
    /// so it is not a new send and must not be rejected.
    #[tokio::test]
    async fn relay_durable_retry_passes_through_full_inbox() {
        let store = broker_store();
        let broker =
            PeerBroker::new(store.clone()).with_peer_inbox_cap(1).with_fold_ack_timeout(Duration::from_millis(1));

        // One send fills the inbox.
        let first = broker
            .relay(peer("a"), peer("b"), durable_wire("env-retry"))
            .await
            .unwrap();
        assert_eq!(first, PeerRouteOutcome::Queued { wake: peer("b") });
        assert_eq!(receiver_mailbox(&store, "b").depth().unwrap(), 1);

        // Retry of the SAME envelope: already in the inbox; `enqueue_unique` is a
        // no-op — the depth does not grow, so the cap check MUST NOT reject.
        let retry = broker
            .relay(peer("a"), peer("b"), durable_wire("env-retry"))
            .await
            .unwrap();
        assert_eq!(
            retry,
            PeerRouteOutcome::Queued { wake: peer("b") },
            "a retry of the same envelope passes through a full inbox (no new entry)"
        );
        assert_eq!(
            receiver_mailbox(&store, "b").depth().unwrap(),
            1,
            "depth unchanged — the retry was idempotent"
        );
    }

    #[test]
    fn test_build_agent_command_with_env() {
        let mut env = HashMap::new();
        env.insert("TEST_KEY".to_string(), "test_value".to_string());

        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            surface_port: DEFAULT_SURFACE_PORT,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: env,
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, None);
        assert!(cmd.contains("TEST_KEY"));
        assert!(cmd.contains("test_value"));
        assert!(cmd.contains("export"));
    }

    // -- SurfaceRouter: per-App surface frame routing (VC-P.1) ---------------

    /// [Verifies VC-P.1] Two Apps registered out of accept order still route
    /// correctly by App identity: a `SurfaceDrive` for App-A reaches App-A's
    /// client stub (not App-B's), and a `SurfaceObservation` from App-A's
    /// client reaches App-A's worker seam (not App-B's). Proves the router is
    /// keyed by App identity, not registration/accept order.
    #[tokio::test]
    async fn surface_router_routes_by_app_identity_not_accept_order() {
        use crate::agent::world::surface::{Hash, IdempotencyKey, Viewport, WindowState};
        use crate::protocol::{AgentToHost, HostToAgent, SurfaceObserved};

        let router = SurfaceRouter::new();

        // Register App-B before App-A — out of alphabetical / accept order —
        // to prove routing does not depend on registration sequence.
        let (drive_tx_b, mut drive_rx_b) = tokio::sync::mpsc::channel::<HostToAgent>(4);
        let (obs_tx_b, mut obs_rx_b) = tokio::sync::mpsc::channel::<AgentToHost>(4);
        router.register_client("app-b".into(), drive_tx_b).await;
        router.register_worker("app-b".into(), obs_tx_b).await;

        let (drive_tx_a, mut drive_rx_a) = tokio::sync::mpsc::channel::<HostToAgent>(4);
        let (obs_tx_a, mut obs_rx_a) = tokio::sync::mpsc::channel::<AgentToHost>(4);
        router.register_client("app-a".into(), drive_tx_a).await;
        router.register_worker("app-a".into(), obs_tx_a).await;

        // A SurfaceDrive for App-A must reach App-A's client stub only.
        let drive_frame = HostToAgent::SurfaceDrive {
            ops: vec![],
            cmd: 1,
            key: IdempotencyKey("cmd-1".into()),
        };
        assert!(
            router.route_drive("app-a", drive_frame).await,
            "route_drive must report delivery to App-A"
        );
        assert!(
            drive_rx_a.recv().await.is_some(),
            "SurfaceDrive must reach App-A's client stub"
        );
        assert!(
            drive_rx_b.try_recv().is_err(),
            "SurfaceDrive for App-A must NOT reach App-B's client stub"
        );

        // A SurfaceObservation from App-A's client must reach App-A's worker
        // seam only, not App-B's.
        let obs_frame = AgentToHost::SurfaceObservation {
            observed: SurfaceObserved {
                surface: 1,
                version: 0,
                ax_digest: Hash("digest-a".into()),
                focus: None,
                selection: None,
                viewport: Viewport(String::new()),
                window: WindowState(String::new()),
                cursor: None,
            },
        };
        assert!(
            router.route_inbound("app-a", obs_frame).await,
            "route_inbound must report delivery to App-A's worker seam"
        );
        assert!(
            obs_rx_a.recv().await.is_some(),
            "SurfaceObservation must reach App-A's worker seam"
        );
        assert!(
            obs_rx_b.try_recv().is_err(),
            "SurfaceObservation for App-A must NOT reach App-B's worker seam"
        );
    }
}
