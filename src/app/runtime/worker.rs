//! The native app-worker entry point.
//!
//! A native app worker is a `rubberduxd` subprocess that drives the ECS World
//! tick-driver ([`crate::agent::world::driver::WorldDriver`]) for a single App
//! and bridges it to the host over the length-prefixed RPC protocol in
//! [`crate::protocol`]. The World driver REPLACES the legacy `AgentLoop` for the
//! agent path: a [`HostToAgent::UserMessage`](crate::protocol::HostToAgent::UserMessage)
//! frame is submitted as a `LogicalInput` into the driver's input queue, the
//! driver folds it through `tick` + the LIVE driver against a `FilesystemEventLog`
//! under the App's session dir, and the assistant text the turn produces is
//! derived from the World's `History` delta and forwarded to the host as
//! [`AgentToHost::EntryNotification`](crate::protocol::AgentToHost::EntryNotification)
//! frames.
//!
//! The first frame it sends is always
//! [`AgentToHost::Hello`](crate::protocol::AgentToHost::Hello) so the host can
//! route this socket to the matching App without relying on accept order. The
//! lifecycle and routing rationale is in
//! `docs/app/runtime/worker-lifecycle.md`. The supervisor that *spawns* this
//! process, the peer broker behind the peer frames, and tombstoning are out of
//! scope here.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::Mutex;

use crate::agent::world::driver::WorldDriver;
use crate::agent::world::effects::{Command, CommandKey, PeerSender, SurfaceDriver};
use crate::agent::world::event_log::FilesystemEventLog;
use crate::agent::world::inputs::{
    Authorization, DeliveryOutcome, DurableEnvelope, LogicalInput, PeerPayload,
};
use crate::agent::world::model_client::MessagesClient;
use crate::agent::world::snapshot::SnapshotStore;
use crate::agent::world::surface::{
    classify_native_signal, IdempotencyKey, NativeSignal, PeerEnvelopeId, SurfaceOp,
};
use crate::agent::world::world::{Effort, ModelConfig, PeerId};
use crate::app::peer::NodeId;
use crate::error::Error;
use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::session::{SessionId, SessionManager};
use crate::tool::peer_message::PeerRequest;

/// Run a native app worker: connect to the host, announce the App with a
/// `Hello` frame, then drive the ECS World tick-driver (`WorldDriver`) —
/// submitting host `UserMessage`s as `LogicalInput`s and forwarding the derived
/// assistant `EntryNotification`s — while bridging surface/peer/interaction
/// frames over RPC.
///
/// `app_session_dir` is the App's on-disk home directory: the worker roots its
/// [`SessionManager`] there so all session data lands under the App's directory
/// rather than the host's home.
pub async fn run_app_worker(rpc_host: String, app_id: String, app_session_dir: PathBuf) {
    if let Err(e) = run_app_worker_inner(&rpc_host, &app_id, app_session_dir).await {
        log::error!("[app-worker:{}] exited with error: {}", app_id, e);
    }
}

async fn run_app_worker_inner(
    rpc_host: &str,
    app_id: &str,
    app_session_dir: PathBuf,
) -> Result<(), Error> {
    let stream = TcpStream::connect(rpc_host)
        .await
        .map_err(|e| Error::Rpc(format!("connect to host {rpc_host}: {e}")))?;
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));

    // The Hello frame must be the first thing on the wire so the host can route
    // this socket to the right App. See docs/app/runtime/worker-lifecycle.md.
    {
        let mut w = writer.lock().await;
        protocol::write_message(
            &mut w,
            &AgentToHost::Hello {
                app_id: app_id.to_string(),
            },
        )
        .await?;
    }

    // Root a SessionManager at the App's home directory so the worker's session
    // data lives under the App, mirroring the in-process supervisor's per-App
    // session creation in crate::app::supervisor.
    let session_manager = Arc::new(session_manager_at(app_session_dir));
    // The agent path is the ECS World tick-driver (crate::agent::world::driver),
    // NOT the legacy AgentLoop: an Anthropic-shape `MessagesClient` performs the
    // model calls the LIVE driver dispatches. See docs/agent/world/ecs-runtime.md.
    let client = MessagesClient::from_env()?;
    let model = world_model_config(client.model());
    // The model alias recorded into a fresh session's metadata, captured before
    // `client` is moved into the driver open below.
    let model_alias = client.model().to_owned();

    // The World driver's INPUT QUEUE: host `UserMessage`s and folded surface
    // inputs are submitted here; the driver task drains it one input at a time,
    // driving each to quiescence. The chat handler never blocks — the bridge
    // pumps only enqueue, never await the drive (root CLAUDE.md UX rule).
    let (world_input_tx, world_input_rx) =
        tokio::sync::mpsc::channel::<LogicalInput>(WORLD_INPUT_CHANNEL_CAPACITY);

    // The peer-messaging transport: the World driver's outbound `WorkerPeerSender`
    // (built below, on the drive path) forwards each `Command::SendPeer` onto
    // `peer_tx`, and `forward_peer_requests` drains `peer_rx`, writing each as an
    // `AgentToHost::PeerSend` the host's broker relays. The send is non-blocking
    // (a bounded `try_send`), so a peer drive never stalls the World tick. See
    // docs/app/peer/decentralized-messaging.md.
    let (peer_tx, peer_rx) = tokio::sync::mpsc::channel::<PeerRequest>(PEER_CHANNEL_CAPACITY);

    // The interaction queue is retained as no-op scaffolding: the World driver
    // raises no interactions yet (interaction-raise outbound is DEFERRED), so the
    // observer idles. `bridge_host_messages` still resolves any inbound
    // `InteractionAnswer` against it. See docs/agent/interaction.md.
    let (interaction_queue, interaction_observer) =
        crate::agent::external::interaction_queue::InteractionQueue::with_observer();
    let interaction_queue = Arc::new(interaction_queue);

    // Outbound channel for `HostToAgent::SurfaceDrive` frames destined for the
    // macOS app client. Frames are queued by the World driver's production
    // `SurfaceDriver` when the agent runs a `set_value` surface tool, and by
    // `bridge_host_messages` when the host relays a surface-drive command;
    // `forward_surface_drives` forwards them to the app writer once it is
    // provisioned by P-host / M-socket. See docs/agent/world/ecs-runtime.md
    // (Theme 2a; SurfaceDrive).
    let (surface_drive_tx, surface_drive_rx) =
        tokio::sync::mpsc::channel::<HostToAgent>(SURFACE_CHANNEL_CAPACITY);

    // Inbound channel for `AgentToHost` surface-observation/mutation frames the
    // host relays from the macOS app client. `bridge_host_messages` re-wraps each
    // inbound `HostToAgent::SurfaceObservation`/`SurfaceMutated` RPC frame as its
    // `AgentToHost` form and pushes it here; `bridge_surface_inbound` folds each
    // into a `LogicalInput` via `bridge_inbound_surface_frame` and submits it into
    // the World input queue. See docs/agent/world/ecs-runtime.md (Theme 2b; Inv 18).
    let (surface_obs_tx, surface_obs_rx) =
        tokio::sync::mpsc::channel::<AgentToHost>(SURFACE_CHANNEL_CAPACITY);

    // Reconstruct the World driver at worker STARTUP — before the message pump
    // (`bridge_host_messages`) begins — so a restarted App resumes its recorded
    // World rather than starting over (US-7): resume the latest existing session
    // via `WorldDriver::open`, falling back to a freshly bootstrapped one when no
    // prior session exists or its recorded tail cannot be reopened. Resolution
    // happens here, not in the per-message handler, so the pump stays non-blocking
    // (root CLAUDE.md UX rule). The `WorkerSurfaceDriver` LIVE surface-drive sink
    // is built inside, rooted on `surface_drive_tx`.
    let driver = open_world_driver(
        &session_manager,
        app_id,
        model,
        model_alias,
        client,
        surface_drive_tx,
    )
    .await?;

    // Drive the ECS World live: drain the input queue, drive each input to
    // quiescence through `tick` + `drive_live`, and forward the derived assistant
    // `EntryNotification`s to the host as RPC frames — the World-derived
    // replacement for the legacy AgentLoop's `OutputPort` forwarding.
    let driver_writer = writer.clone();
    let driver_app_id = app_id.to_string();
    // The World driver's outbound peer sink: forwards each `Command::SendPeer` to
    // the host broker over `peer_tx` (replacing the inert `UnattachedPeerSender` on
    // this worker's drive path). It owns `peer_tx`, keeping the forwarder's `peer_rx`
    // open for the worker's lifetime. See docs/agent/world/ecs-runtime.md.
    let peer_sender = WorkerPeerSender {
        peer_tx,
        from: app_id.to_string(),
    };
    let driver_task = tokio::spawn(async move {
        run_world_driver(driver, world_input_rx, driver_writer, peer_sender, &driver_app_id).await;
    });

    // Worker → host for raised interactions: drain the queue's observer and write
    // each as an `AgentToHost::Interaction` frame. No-op for this milestone (the
    // World driver raises none), retained for later interaction-raise wiring.
    let interaction_writer = writer.clone();
    let interaction_app_id = app_id.to_string();
    let interaction_task = tokio::spawn(async move {
        forward_interactions(interaction_observer, interaction_writer, &interaction_app_id).await;
    });

    // Pending `peer_list` replies, correlated FIFO: a `PeerList` frame carries no
    // request id, and the worker issues them one at a time per tool call, so the
    // oldest unanswered request matches the next `PeerListResult`.
    let pending_lists: PendingLists = Arc::new(Mutex::new(std::collections::VecDeque::new()));

    // Worker → host for peer frames: drain the peer-tool requests, writing each as
    // an `AgentToHost` frame. No-op for this milestone (the World driver raises no
    // peer requests), retained for later peer-send wiring.
    let peer_writer = writer.clone();
    let peer_app_id = app_id.to_string();
    let peer_pending = pending_lists.clone();
    let peer_task = tokio::spawn(async move {
        forward_peer_requests(peer_rx, peer_writer, peer_pending, &peer_app_id).await;
    });

    // Relay outbound surface drives to the host as `AgentToHost::SurfaceDrive`
    // frames; the host's SurfaceRouter routes each on to the registered macOS
    // client. Writes share the host RPC writer with the driver/interaction pumps.
    let surface_drive_app_id = app_id.to_string();
    let surface_drive_writer = writer.clone();
    let surface_drive_task = tokio::spawn(async move {
        forward_surface_drives(surface_drive_rx, surface_drive_writer, &surface_drive_app_id).await;
    });

    // Fold inbound surface frames into LogicalInputs and submit them into the
    // World input queue (Inv 18, Theme 2b).
    let surface_obs_app_id = app_id.to_string();
    let surface_obs_world_tx = world_input_tx.clone();
    let surface_obs_task = tokio::spawn(async move {
        bridge_surface_inbound(surface_obs_rx, &surface_obs_app_id, surface_obs_world_tx).await;
    });

    // Bridge host → worker: submit user messages into the World input queue,
    // deliver peer messages as turns, fulfill peer_list answers, route interaction
    // answers back into the queue, fold relayed surface observations into the World
    // input queue, and honor shutdown.
    bridge_host_messages(
        &mut reader,
        &world_input_tx,
        app_id,
        &pending_lists,
        &interaction_queue,
        &surface_obs_tx,
    )
    .await;

    driver_task.abort();
    interaction_task.abort();
    peer_task.abort();
    surface_drive_task.abort();
    surface_obs_task.abort();
    Ok(())
}

/// Capacity of the World driver's input queue. Inputs arrive at human interaction
/// speed (user messages, folded surface signals); a small buffer absorbs bursts
/// without back-pressuring the non-blocking bridge pumps.
const WORLD_INPUT_CHANNEL_CAPACITY: usize = 64;

/// Build the World-default [`ModelConfig`] from the configured model alias. The
/// `max_tokens` budget is read from `RUBBERDUX_LLM_MAX_TOKENS` (so it is config,
/// not a hardcoded constant) and defaults to a sane ceiling; effort defaults to
/// `Medium`. The alias rides in the `/v1/messages` request body the LIVE driver
/// assembles. See docs/agent/world/ecs-runtime.md (Anthropic model-call mapping).
fn world_model_config(model: &str) -> ModelConfig {
    let max_tokens = std::env::var("RUBBERDUX_LLM_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(4096);
    ModelConfig {
        model: model.to_string(),
        max_tokens,
        effort: Effort::Medium,
    }
}

/// Derive the World's deterministic RNG seed from the App and session ids — a
/// pure FNV-1a over `"{app_id}:{session_id}"`. The seed is recorded once in the
/// `SessionStarted` header so a replay reseeds identically (Inv 8/9); deriving it
/// purely (no ambient entropy) keeps a fresh session's genesis reproducible from
/// its ids alone. See docs/agent/world/ecs-runtime.md (Rng; SessionStarted).
fn seed_from_session(app_id: &str, session_id: &SessionId) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in format!("{app_id}:{}", session_id.to_string()).bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The production [`SurfaceDriver`]: the World driver's LIVE surface-drive sink.
/// When the agent runs a `set_value` surface `RunTool`, `drive_live` hands the
/// Command here; this converts it to a `HostToAgent::SurfaceDrive` frame (via
/// `surface_drive_frame`) and forwards it to the macOS app client over the
/// drive channel so the screen actually changes. A full or dropped receiver
/// (pre-P-host / M-socket) drops the frame silently — never blocks or errors.
/// See docs/agent/world/ecs-runtime.md (Theme 2a; SurfaceDrive).
struct WorkerSurfaceDriver {
    tx: tokio::sync::mpsc::Sender<HostToAgent>,
}

impl SurfaceDriver for WorkerSurfaceDriver {
    async fn drive(&self, command: &Command) -> Result<(), Error> {
        if let Some(frame) = surface_drive_frame(command) {
            let _ = self.tx.try_send(frame);
        }
        Ok(())
    }
}

/// The subprocess worker's [`PeerSender`]: the real outbound peer sink that REPLACES
/// the inert `UnattachedPeerSender` on the World driver's drive path. When the agent
/// emits a `Command::SendPeer`, `drive_live` hands it here; this wraps the drive in a
/// [`DurableEnvelope`] (so the receiving worker reconstructs the inbound peer input
/// verbatim) and forwards it onto `peer_tx` as a [`PeerRequest::Send`], which
/// `forward_peer_requests` writes as an `AgentToHost::PeerSend` the host's broker
/// relays (`local_supervisor` → `PeerBroker::relay`). The forward is non-blocking (a
/// bounded `try_send`) so a peer drive never stalls the tick. It reports
/// [`DeliveryOutcome::Queued`] best-effort: the AUTHORITATIVE routing verdict lives in
/// the host broker (an in-process host injects the broker-backed `BrokerPeerSender`,
/// which returns the real outcome); recording a durable ack is a later pass. See
/// docs/agent/world/ecs-runtime.md (§645; the World↔broker boundary).
struct WorkerPeerSender {
    /// The worker's peer-request channel into `forward_peer_requests`.
    peer_tx: tokio::sync::mpsc::Sender<PeerRequest>,
    /// This worker's App id — the sender identity stamped into `auth.from`.
    from: String,
}

impl PeerSender for WorkerPeerSender {
    async fn send(
        &self,
        to: &PeerId,
        payload: &PeerPayload,
        _key: &CommandKey,
        envelope: &PeerEnvelopeId,
    ) -> Result<DeliveryOutcome, Error> {
        let durable = DurableEnvelope {
            id: envelope.clone(),
            payload: payload.clone(),
            auth: Authorization {
                from: PeerId {
                    app_id: self.from.clone(),
                    node_id: NodeId::LOCAL.to_string(),
                },
                // Shape-level token (verification is a later enforcement pass).
                token: self.from.clone(),
            },
        };
        let wire = serde_json::to_value(&durable)?;
        // Fire-and-forget onto the host: a full/closed channel drops the drive rather
        // than blocking the tick (best-effort; the durable broker hardens this).
        let _ = self.peer_tx.try_send(PeerRequest::Send {
            to: to.app_id.clone(),
            payload: wire,
        });
        Ok(DeliveryOutcome::Queued)
    }
}

/// Run the World tick-driver loop over an already-opened [`WorldDriver`]
/// (resumed or freshly bootstrapped at startup by [`open_world_driver`]): drain
/// `world_input_rx`, driving each input to quiescence and forwarding the derived
/// assistant `EntryNotification`s to the host as `AgentToHost::EntryNotification`
/// frames so the macOS app renders the agent's reply. A per-input drive error is
/// logged and the loop continues with the next input.
async fn run_world_driver(
    mut driver: WorldDriver<MessagesClient, WorkerSurfaceDriver, FilesystemEventLog>,
    mut world_input_rx: tokio::sync::mpsc::Receiver<LogicalInput>,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    peer_sender: WorkerPeerSender,
    app_id: &str,
) {
    while let Some(input) = world_input_rx.recv().await {
        // The inbound peer envelope this input folds (captured BEFORE the input is
        // moved into submit): a successful submit has durably appended the
        // `DriveRequested`/`PeerDelivered` to the World's stratum-1 log, so the
        // worker can ack the DURABLE fold back to the host broker — the receiver→
        // broker confirmation that makes the sender's `Delivered` non-optimistic
        // (INV-3) and advances the durable inbox (INV-4). A redelivery the receiver's
        // stratum-1 dedup folded as a NO-OP is STILL acked so the broker advances
        // past it. See docs/agent/world/ecs-runtime.md §"Durable peer delivery".
        let folded_envelope = peer_envelope_of(&input);
        // Drive each input under the real peer sender so a `Command::SendPeer`
        // reaches the host broker (not the inert `UnattachedPeerSender`).
        match driver.submit_with_peer_sender(input, &peer_sender).await {
            Ok(notifications) => {
                if let Some(envelope) = folded_envelope {
                    let ack = AgentToHost::PeerDeliverAck { envelope: envelope.0 };
                    let mut w = writer.lock().await;
                    if let Err(e) = protocol::write_message(&mut w, &ack).await {
                        log::error!(
                            "[app-worker:{}] failed to ack peer fold: {}",
                            app_id,
                            e
                        );
                        return;
                    }
                }
                for notification in notifications {
                    let frame = AgentToHost::EntryNotification {
                        entry: notification.entry,
                        is_final: notification.is_final,
                    };
                    let mut w = writer.lock().await;
                    if let Err(e) = protocol::write_message(&mut w, &frame).await {
                        log::error!(
                            "[app-worker:{}] failed to forward entry notification: {}",
                            app_id,
                            e
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                log::error!("[app-worker:{}] World driver tick failed: {}", app_id, e);
            }
        }
    }
}

/// The concrete [`WorldDriver`] the production worker drives: an Anthropic-shape
/// [`MessagesClient`] model caller, the [`WorkerSurfaceDriver`] LIVE surface sink,
/// and a [`FilesystemEventLog`] rooted under the session dir.
type WorkerWorldDriver = WorldDriver<MessagesClient, WorkerSurfaceDriver, FilesystemEventLog>;

/// Open the World driver this worker drives, at STARTUP (before the message pump):
/// RESUME the latest existing session via [`WorldDriver::open`] so a restarted App
/// continues its recorded World (US-7), or — when no prior session exists, or the
/// recorded tail cannot be reopened (e.g. the documented crash-mid-compaction P0
/// limitation `open` reports as `Err`) — create and bootstrap a FRESH session so
/// the worker always stays available. The reopen failure is logged, never fatal.
///
/// A resumed session reuses the SAME `world-events.jsonl` directory (the `latest`
/// link resolves it) rather than starting a new one, and roots a [`SnapshotStore`]
/// under that session dir's `snapshots/` so the driver's periodic snapshots land
/// alongside the log. See docs/agent/world/ecs-runtime.md — Restore snapshots.
async fn open_world_driver(
    session_manager: &SessionManager,
    app_id: &str,
    model: ModelConfig,
    model_alias: String,
    client: MessagesClient,
    surface_drive_tx: tokio::sync::mpsc::Sender<HostToAgent>,
) -> Result<WorkerWorldDriver, Error> {
    // Prefer resuming the latest existing session.
    if let Some((session_id, session_dir)) = latest_session(session_manager) {
        let event_log = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
        let store = SnapshotStore::new(session_dir.join("snapshots"));
        let seed = seed_from_session(app_id, &session_id);
        let surface_driver = WorkerSurfaceDriver {
            tx: surface_drive_tx.clone(),
        };
        match WorldDriver::open(seed, model.clone(), client, surface_driver, event_log, store).await
        {
            Ok(driver) => {
                log::info!(
                    "[app-worker:{}] resumed session {} from its recorded World",
                    app_id,
                    session_id.to_string()
                );
                return Ok(driver);
            }
            Err(e) => {
                // `client` was consumed by the failed `open`; rebuild it from env
                // for the fresh session below so the worker stays available.
                log::warn!(
                    "[app-worker:{}] could not reopen latest session {} ({}); \
                     falling back to a fresh session",
                    app_id,
                    session_id.to_string(),
                    e
                );
                let client = MessagesClient::from_env()?;
                return open_fresh(session_manager, app_id, model, model_alias, client, surface_drive_tx)
                    .await;
            }
        }
    }

    // No prior session: create + bootstrap a fresh one with the existing client.
    open_fresh(session_manager, app_id, model, model_alias, client, surface_drive_tx).await
}

/// Create a fresh session and open a freshly-bootstrapped [`WorldDriver`] over its
/// (empty) log, rooting a [`SnapshotStore`] under the new session dir. `open` on an
/// empty log delegates to `bootstrap` (genesis + recorded `SessionStarted`) and
/// merely attaches the store, so periodic snapshots still land under the session.
async fn open_fresh(
    session_manager: &SessionManager,
    app_id: &str,
    model: ModelConfig,
    model_alias: String,
    client: MessagesClient,
    surface_drive_tx: tokio::sync::mpsc::Sender<HostToAgent>,
) -> Result<WorkerWorldDriver, Error> {
    let (session_id, session_dir) = session_manager
        .create_session(model_alias)
        .map_err(|e| Error::App(format!("create session for App `{app_id}`: {e}")))?;

    // The World's append-only event log lives under the session directory: the
    // single source of truth the driver appends to log-before-apply (Inv 4/9).
    // Its stratum-2 sibling (`<name>.lifecycle.jsonl`) travels alongside it.
    let event_log = FilesystemEventLog::new(session_dir.join("world-events.jsonl"));
    let store = SnapshotStore::new(session_dir.join("snapshots"));
    // A deterministic per-session RNG seed: the World's sole randomness source
    // crosses the recorded `SessionStarted` boundary so a replay reseeds
    // identically (Inv 8/9). Derived purely from the App + session ids.
    let seed = seed_from_session(app_id, &session_id);
    let surface_driver = WorkerSurfaceDriver {
        tx: surface_drive_tx,
    };
    WorldDriver::open(seed, model, client, surface_driver, event_log, store).await
}

/// Resolve the worker's most recent existing session from the `latest` link:
/// `Some((session_id, session_dir))` when the link resolves to a session directory
/// whose name parses as a [`SessionId`], else `None` (no prior session to resume).
fn latest_session(session_manager: &SessionManager) -> Option<(SessionId, PathBuf)> {
    let target = std::fs::read_link(session_manager.latest_link()).ok()?;
    let name = target.file_name()?.to_str()?;
    let session_id = SessionId::from_string(name)?;
    let session_dir = session_manager.session_dir(&session_id);
    session_dir.is_dir().then_some((session_id, session_dir))
}

/// Capacity of the worker's peer-request channel. Peer tool calls are serviced
/// promptly by the forwarder task, so a small buffer absorbs bursts.
const PEER_CHANNEL_CAPACITY: usize = 32;

/// Capacity of the surface-frame channels (outbound drive and inbound
/// observation). Surface events arrive at human interaction speed so a small
/// buffer absorbs transient bursts without back-pressure on the bridge tasks.
const SURFACE_CHANNEL_CAPACITY: usize = 32;

/// Shared queue of `peer_list` reply channels awaiting their `PeerListResult`,
/// correlated first-in-first-out. Shared between the peer-request forwarder
/// (which enqueues) and the host-message bridge (which dequeues on a result).
type PendingLists = Arc<Mutex<std::collections::VecDeque<tokio::sync::oneshot::Sender<Vec<String>>>>>;

/// Drain the peer-tool request channel, writing each request to the host as the
/// matching `AgentToHost` frame. For a `List`, the reply oneshot is recorded in
/// `pending` first so the bridge can fulfill it on the host's `PeerListResult`.
async fn forward_peer_requests(
    mut peer_rx: tokio::sync::mpsc::Receiver<PeerRequest>,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    pending: PendingLists,
    app_id: &str,
) {
    while let Some(request) = peer_rx.recv().await {
        let frame = match request {
            PeerRequest::Send { to, payload } => AgentToHost::PeerSend { to, payload },
            PeerRequest::List { reply } => {
                // Record the reply before sending so a fast `PeerListResult` is
                // never observed before its waiter is enqueued.
                pending.lock().await.push_back(reply);
                AgentToHost::PeerList
            }
        };
        let mut w = writer.lock().await;
        if let Err(e) = protocol::write_message(&mut w, &frame).await {
            log::error!("[app-worker:{}] failed to forward peer frame: {}", app_id, e);
            break;
        }
    }
}

/// Drain the interaction queue's observer, forwarding each raised request to the
/// host as an [`AgentToHost::Interaction`] frame. The legacy `UIInteractionRequest`
/// is converted to the unified `AgentInteraction` the host pump consumes. A
/// `Lagged` notice is logged and skipped; a closed channel ends the pump.
async fn forward_interactions(
    mut observer: tokio::sync::broadcast::Receiver<
        crate::agent::external::UIInteractionRequest,
    >,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    app_id: &str,
) {
    loop {
        match observer.recv().await {
            Ok(request) => {
                let interaction: crate::agent::interaction::AgentInteraction = request.into();
                let frame = AgentToHost::Interaction { interaction };
                let mut w = writer.lock().await;
                if let Err(e) = protocol::write_message(&mut w, &frame).await {
                    log::error!(
                        "[app-worker:{}] failed to forward interaction: {}",
                        app_id,
                        e
                    );
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                log::warn!(
                    "[app-worker:{}] interaction observer lagged, skipped {} requests",
                    app_id,
                    skipped
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Build a [`SessionManager`] rooted at `home`, honoring the `--app-session-dir`
/// flag without mutating process-global environment. Mirrors
/// [`SessionManager::new`]'s directory layout.
fn session_manager_at(home: PathBuf) -> SessionManager {
    let sessions_dir = home.join("sessions");
    let latest_link = home.join("latest");
    SessionManager {
        home_dir: home,
        sessions_dir,
        latest_link,
    }
}

/// Convert an inbound surface frame received from the macOS app client into a
/// `LogicalInput` for the ECS World, applying echo-dedup (Theme 1e / Inv 18).
///
/// - `SurfaceObservation` → `SurfaceObserved` perceived-state snapshot (always
///   folded as a free-variable EXOGENOUS input; Theme 2b).
/// - `SurfaceMutated` → `SurfaceMutated` Input ONLY when
///   `classify_native_signal` returns `LogAsMutation` (i.e. `cause == Human`);
///   `Command`/`Peer`-caused echoes return `None` and are DROPPED — they are
///   already recorded as the originating `ToolReturned`/`DriveRequested` and
///   must not be double-counted (Inv 18). See docs/agent/world/ecs-runtime.md
///   (Theme 1e; Native echo dedup).
/// - All other `AgentToHost` variants return `None` (not surface frames).
fn bridge_inbound_surface_frame(frame: AgentToHost) -> Option<LogicalInput> {
    match frame {
        AgentToHost::SurfaceObservation { observed } => Some(LogicalInput::SurfaceObserved {
            surface: observed.surface,
            version: observed.version,
            ax_digest: observed.ax_digest,
            focus: observed.focus,
            selection: observed.selection,
            viewport: observed.viewport,
            window: observed.window,
            cursor: observed.cursor,
        }),
        AgentToHost::SurfaceMutated { op, cause } => match classify_native_signal(&cause) {
            NativeSignal::LogAsMutation => Some(LogicalInput::SurfaceMutated { op }),
            NativeSignal::DropEcho => {
                // Drop silently — the agent write is already recorded as the
                // ToolReturned of the originating set_value RunTool (Inv 18).
                log::debug!(
                    "[surface-bridge] dropping echo (cause={cause:?}) — already in ToolReturned"
                );
                None
            }
        },
        _ => None,
    }
}

/// Bridge an inbound `HostToAgent::PeerDeliver { from, payload }` into the World's
/// peer-edge input — REPLACING the legacy flattening to `UserMessage { to: 0 }`. The
/// broker relays the sender's [`DurableEnvelope`] (id + payload + auth) as `payload`:
///
/// - `PeerPayload::Drive` → `LogicalInput::DriveRequested` (the surface ops + prompt
///   the target's PeerDriveSystem applies as a projection + Inbox enqueue), and
/// - `PeerPayload::Message` → `LogicalInput::PeerDelivered` (a generic peer message).
///
/// `from` is the broker's authoritative sender App id on the local node, rebuilt as a
/// World-side [`PeerId`] so the driver stamps the input `Origin::Peer` on
/// `edge_for(Peer(from))` and PeerDriveSystem's authorization (`auth.from == from`)
/// resolves. A payload that is NOT a `DurableEnvelope` (a legacy/plain message, e.g. a
/// drained inbox entry) is delivered as `PeerDelivered` carrying the raw payload under
/// a sender-derived envelope id. See docs/agent/world/ecs-runtime.md (Theme 4a/4b;
/// DriveRequested; PeerDelivered).
/// The inbound peer envelope this `LogicalInput` folds, if any: a `DriveRequested`
/// or `PeerDelivered` carries the sender-assigned [`PeerEnvelopeId`] the worker acks
/// back to the host broker after the durable fold (the receiver→broker durable-fold
/// confirmation, INV-3/INV-4). `None` for every non-peer input.
fn peer_envelope_of(input: &LogicalInput) -> Option<PeerEnvelopeId> {
    match input {
        LogicalInput::DriveRequested { envelope, .. }
        | LogicalInput::PeerDelivered { envelope, .. } => Some(envelope.clone()),
        _ => None,
    }
}

fn bridge_peer_deliver(from: &str, payload: serde_json::Value) -> LogicalInput {
    let from_peer = PeerId {
        app_id: from.to_string(),
        node_id: NodeId::LOCAL.to_string(),
    };
    match serde_json::from_value::<DurableEnvelope>(payload.clone()) {
        Ok(envelope) => match envelope.payload {
            PeerPayload::Drive(drive) => LogicalInput::DriveRequested {
                from: from_peer,
                envelope: envelope.id,
                drive,
                auth: envelope.auth,
            },
            PeerPayload::Message(message) => LogicalInput::PeerDelivered {
                from: from_peer,
                envelope: envelope.id,
                payload: message,
            },
        },
        // A legacy/plain payload carries no durable envelope: deliver it as a generic
        // peer message keyed by a sender-derived envelope id (best-effort — durable
        // idempotency is the broker's later pass).
        Err(_) => LogicalInput::PeerDelivered {
            envelope: PeerEnvelopeId(format!("{from}/{}/legacy", NodeId::LOCAL)),
            from: from_peer,
            payload,
        },
    }
}

/// Convert an ECS-World surface-drive Command to a `HostToAgent::SurfaceDrive`
/// frame for dispatch to the macOS app client. Returns `None` when the Command
/// is not a UI surface drive (i.e. `tool != "set_value"`). The `cmd` and `key`
/// are forwarded so the app can stamp the resulting native UI-change echo with
/// `Cause::Command { cmd, key }` for echo dedup (Theme 1e / Inv 18). The
/// `surface_ops` are extracted from the tool's `args["surface_ops"]` JSON field
/// (the canonical encoding the set_value tool uses). See
/// docs/agent/world/ecs-runtime.md (Theme 2a; SurfaceDrive).
fn surface_drive_frame(command: &Command) -> Option<HostToAgent> {
    let Command::RunTool { cmd, tool, args, .. } = command else {
        return None;
    };
    if tool != "set_value" {
        return None;
    }
    let ops: Vec<SurfaceOp> = args
        .get("surface_ops")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    // P0: CommandKey is a unit-struct placeholder; the full IdempotencyKey
    // { app_id, tick, effect_id } arrives with the WAL milestone (P2-wal).
    let key = IdempotencyKey(format!("cmd-{cmd}"));
    Some(HostToAgent::SurfaceDrive { ops, cmd: *cmd, key })
}

/// Read host frames until disconnect or shutdown, bridging them into the worker:
/// a `UserMessage` becomes a `LogicalInput::UserMessage` submitted to the World
/// driver's input queue; a `PeerDeliver` is mapped by `bridge_peer_deliver` to an
/// `Origin::Peer` `DriveRequested`/`PeerDelivered` on the peer edge (no longer
/// flattened to a `UserMessage`); a `PeerListResult` fulfills the oldest pending
/// `peer_list`. A relayed `SurfaceObservation`/`SurfaceMutated` (the host→worker
/// leg of the surface relay) is re-wrapped as its `AgentToHost` form and pushed
/// onto `surface_obs_tx`, where `bridge_surface_inbound` folds it into the World
/// input queue. Other frames are logged. Submitting only ENQUEUES (the driver task
/// drives the turn), so the host-message pump never blocks (root CLAUDE.md UX rule).
async fn bridge_host_messages(
    reader: &mut OwnedReadHalf,
    world_input_tx: &tokio::sync::mpsc::Sender<LogicalInput>,
    app_id: &str,
    pending_lists: &PendingLists,
    interaction_queue: &Arc<crate::agent::external::interaction_queue::InteractionQueue>,
    surface_obs_tx: &tokio::sync::mpsc::Sender<AgentToHost>,
) {
    loop {
        match protocol::read_message::<HostToAgent>(reader).await {
            Ok(Some(HostToAgent::UserMessage { text, .. })) => {
                // Address the primary entity (entity 0); IntakeSystem admits it and
                // starts the turn. Submitting only enqueues — never blocks the pump.
                match world_input_tx.try_send(LogicalInput::UserMessage { to: 0, text }) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        log::warn!(
                            "[app-worker:{}] World input queue full; user message dropped",
                            app_id
                        );
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        log::error!(
                            "[app-worker:{}] World input channel closed; stopping pump",
                            app_id
                        );
                        break;
                    }
                }
            }
            Ok(Some(HostToAgent::PeerDeliver { from, payload })) => {
                // A peer message becomes an `Origin::Peer` input on the peer edge — a
                // cross-World `DriveRequested` or a generic `PeerDelivered` — NEVER
                // flattened to a `UserMessage` (the World records A acting inside B in
                // B's own log, Theme 4b). The driver stamps `Origin::Peer` on
                // `edge_for(Peer(from))`. Submitting only ENQUEUES, so the pump stays
                // non-blocking (root CLAUDE.md UX rule).
                let input = bridge_peer_deliver(&from, payload);
                match world_input_tx.try_send(input) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        log::warn!(
                            "[app-worker:{}] World input queue full; peer message from {} dropped",
                            app_id, from
                        );
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        log::error!(
                            "[app-worker:{}] World input channel closed; stopping pump",
                            app_id
                        );
                        break;
                    }
                }
            }
            Ok(Some(HostToAgent::PeerListResult { peers })) => {
                // Fulfill the oldest unanswered peer_list. A result with no waiter
                // (a late/duplicate answer) is dropped.
                if let Some(reply) = pending_lists.lock().await.pop_front() {
                    let _ = reply.send(peers);
                } else {
                    log::debug!("[app-worker:{}] PeerListResult with no waiter", app_id);
                }
            }
            Ok(Some(HostToAgent::InteractionAnswer { response })) => {
                // The host answered a raised interaction. Convert the unified
                // response back to the legacy shape the queue resolves on, then
                // unblock the awaiting raise tool by its request id. A miss means
                // the interaction already cleared (late/duplicate answer).
                let request_id = response.request_id().to_string();
                let legacy: crate::agent::external::UIInteractionResponse = response.into();
                if !interaction_queue.resolve(&request_id, legacy) {
                    log::debug!(
                        "[app-worker:{}] InteractionAnswer for unknown request {}",
                        app_id,
                        request_id
                    );
                }
            }
            Ok(Some(HostToAgent::SurfaceObservation { observed })) => {
                // The host relayed a macOS client's observed AX snapshot (the
                // host→worker leg of the surface relay). Re-wrap it as the
                // `AgentToHost` form `bridge_inbound_surface_frame` folds and hand
                // it to the surface-inbound pump, which submits it into the World
                // input queue. A closed/full queue drops it (never blocks).
                // See docs/agent/world/ecs-runtime.md (Theme 2b; Inv 18).
                let frame = AgentToHost::SurfaceObservation { observed };
                if surface_obs_tx.try_send(frame).is_err() {
                    log::warn!(
                        "[app-worker:{}] surface inbound queue unavailable; observation dropped",
                        app_id
                    );
                }
            }
            Ok(Some(HostToAgent::SurfaceMutated { op, cause })) => {
                // The host relayed a macOS client's UI mutation. Re-wrap it as the
                // `AgentToHost` form; `bridge_inbound_surface_frame` keeps only
                // `cause = Human` (Command/Peer echoes are deduped per Inv 18).
                let frame = AgentToHost::SurfaceMutated { op, cause };
                if surface_obs_tx.try_send(frame).is_err() {
                    log::warn!(
                        "[app-worker:{}] surface inbound queue unavailable; mutation dropped",
                        app_id
                    );
                }
            }
            Ok(Some(HostToAgent::Shutdown)) => {
                log::info!("[app-worker:{}] received shutdown", app_id);
                break;
            }
            Ok(Some(other)) => {
                log::debug!("[app-worker:{}] unhandled host frame: {:?}", app_id, other);
            }
            Ok(None) => {
                log::info!("[app-worker:{}] host disconnected", app_id);
                break;
            }
            Err(e) => {
                log::error!("[app-worker:{}] error reading from host: {}", app_id, e);
                break;
            }
        }
    }
}

/// Relay the worker's outbound surface drives to the host. Frames arrive on the
/// drive channel as `HostToAgent::SurfaceDrive` (the host→app form produced by
/// `WorkerSurfaceDriver`/`surface_drive_frame`); each is repackaged as the relay
/// frame `AgentToHost::SurfaceDrive` and written to the host RPC writer, where the
/// host's `SurfaceRouter` routes it on to the registered macOS client (keyed by
/// App id). A write failure ends the pump; the worker is restarted by its
/// supervisor. See docs/agent/world/ecs-runtime.md (Theme 2a; SurfaceDrive).
async fn forward_surface_drives(
    mut rx: tokio::sync::mpsc::Receiver<HostToAgent>,
    writer: Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    app_id: &str,
) {
    while let Some(frame) = rx.recv().await {
        let HostToAgent::SurfaceDrive { ops, cmd, key } = frame else {
            // Only `SurfaceDrive` frames ride this channel; ignore defensively.
            log::debug!(
                "[app-worker:{}] non-drive frame on surface drive channel; dropping: {:?}",
                app_id,
                frame
            );
            continue;
        };
        let relay = AgentToHost::SurfaceDrive { ops, cmd, key };
        let mut w = writer.lock().await;
        if let Err(e) = protocol::write_message(&mut w, &relay).await {
            log::error!(
                "[app-worker:{}] failed to forward surface drive to host: {}",
                app_id,
                e
            );
            break;
        }
    }
}

/// Fold inbound `AgentToHost` surface frames from the macOS app into
/// `LogicalInput`s via [`bridge_inbound_surface_frame`] and submit each into the
/// World driver's input queue (Inv 18, Theme 2b).
///
/// - `SurfaceObservation` → `SurfaceObserved` (always forwarded).
/// - `SurfaceMutated{Human}` → `SurfaceMutated` (forwarded).
/// - `SurfaceMutated{Command|Peer}` → dropped (echo dedup per Inv 18).
/// - All other frames → `None`, dropped.
///
/// A closed input queue (the driver task exited) ends the pump.
async fn bridge_surface_inbound(
    mut rx: tokio::sync::mpsc::Receiver<AgentToHost>,
    app_id: &str,
    world_input_tx: tokio::sync::mpsc::Sender<LogicalInput>,
) {
    while let Some(frame) = rx.recv().await {
        if let Some(input) = bridge_inbound_surface_frame(frame) {
            match world_input_tx.try_send(input) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    log::warn!(
                        "[app-worker:{}] World input queue full; surface observation dropped",
                        app_id
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    log::debug!(
                        "[app-worker:{}] World input queue closed; ending surface pump",
                        app_id
                    );
                    break;
                }
            }
        }
        // None → Command/Peer echo dropped per Inv 18 (DropEcho path).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::effects::CommandKey;
    use crate::agent::world::surface::{Cause, Hash, PeerEnvelopeId, Selection, Viewport, WindowState};
    use crate::protocol::SurfaceObserved;

    #[test]
    fn session_manager_at_roots_layout_under_home() {
        let home = PathBuf::from("/tmp/app-123");
        let mgr = session_manager_at(home.clone());
        assert_eq!(mgr.home_dir, home);
        assert_eq!(mgr.sessions_dir, home.join("sessions"));
        assert_eq!(mgr.latest_link, home.join("latest"));
    }

    /// [Verifies VC-P.1] A `SurfaceMutated` frame with `cause = Human` is folded
    /// into a `LogicalInput::SurfaceMutated` carrying the same op (Inv 18).
    #[test]
    fn surface_mutated_human_cause_produces_surface_mutated_input() {
        let op = SurfaceOp::SetValue {
            surface: 1,
            element: 2,
            value: serde_json::json!("typed by hand"),
            base_version: None,
        };
        let frame = AgentToHost::SurfaceMutated {
            op: op.clone(),
            cause: Cause::Human,
        };
        match bridge_inbound_surface_frame(frame) {
            Some(LogicalInput::SurfaceMutated { op: got }) => {
                assert_eq!(got, op, "the op must pass through unchanged");
            }
            other => panic!("expected SurfaceMutated Input, got {:?}", other),
        }
    }

    /// [Verifies VC-P.1] A `SurfaceMutated` frame with `cause = Command` is
    /// DROPPED (returns `None`) — it is an echo of an agent set_value that is
    /// already recorded as a `ToolReturned` (Inv 18, Theme 1e).
    #[test]
    fn surface_mutated_command_cause_is_dropped() {
        let frame = AgentToHost::SurfaceMutated {
            op: SurfaceOp::Click {
                surface: 3,
                element: 9,
                point: None,
                base_version: None,
            },
            cause: Cause::Command {
                cmd: 7,
                key: IdempotencyKey("tick-7-effect-0".into()),
            },
        };
        assert!(
            bridge_inbound_surface_frame(frame).is_none(),
            "a Command-caused echo must be dropped (Inv 18)"
        );
    }

    /// [Verifies VC-P.1] A `SurfaceMutated` frame with `cause = Peer` is also
    /// DROPPED — it is an echo of a DriveRequested (Inv 18).
    #[test]
    fn surface_mutated_peer_cause_is_dropped() {
        let frame = AgentToHost::SurfaceMutated {
            op: SurfaceOp::Navigate {
                surface: 2,
                route: crate::agent::world::surface::Route("home".into()),
                base_version: None,
            },
            cause: Cause::Peer {
                envelope: PeerEnvelopeId("env-xyz".into()),
            },
        };
        assert!(
            bridge_inbound_surface_frame(frame).is_none(),
            "a Peer-caused echo must be dropped (Inv 18)"
        );
    }

    /// [Verifies VC-P.1] A `SurfaceObservation` frame is converted to a
    /// `LogicalInput::SurfaceObserved` carrying the full perceived AX snapshot
    /// (Theme 2b).
    #[test]
    fn surface_observation_frame_produces_surface_observed_input() {
        let observed = SurfaceObserved {
            surface: 5,
            version: 42,
            ax_digest: Hash("digest-abc".into()),
            focus: Some(7),
            selection: Some(Selection("0-4".into())),
            viewport: Viewport("rect".into()),
            window: WindowState("key".into()),
            cursor: Some((100, 200)),
        };
        let frame = AgentToHost::SurfaceObservation { observed };
        match bridge_inbound_surface_frame(frame) {
            Some(LogicalInput::SurfaceObserved {
                surface,
                version,
                ax_digest,
                focus,
                cursor,
                ..
            }) => {
                assert_eq!(surface, 5);
                assert_eq!(version, 42);
                assert_eq!(ax_digest, Hash("digest-abc".into()));
                assert_eq!(focus, Some(7));
                assert_eq!(cursor, Some((100, 200)));
            }
            other => panic!("expected SurfaceObserved Input, got {:?}", other),
        }
    }

    /// [Verifies VC-P.1] A `set_value` `RunTool` Command produces a
    /// `HostToAgent::SurfaceDrive` frame carrying the correct `cmd` and a
    /// non-empty `key`, so the macOS app can stamp the resulting native echo
    /// with `Cause::Command { cmd, key }` (Theme 1e / Inv 18).
    #[test]
    fn run_tool_set_value_produces_surface_drive_frame() {
        let op = SurfaceOp::SetValue {
            surface: 2,
            element: 5,
            value: serde_json::json!("hello"),
            base_version: Some(3),
        };
        let command = Command::RunTool {
            cmd: 9,
            entity: 0,
            tool: "set_value".into(),
            args: serde_json::json!({
                "surface_ops": serde_json::to_value(&[op]).unwrap()
            }),
            key: CommandKey,
        };
        match surface_drive_frame(&command) {
            Some(HostToAgent::SurfaceDrive { ops, cmd, key }) => {
                assert_eq!(cmd, 9, "cmd must be forwarded");
                assert_eq!(ops.len(), 1, "ops must be forwarded");
                assert!(!key.0.is_empty(), "key must be non-empty (P0 placeholder)");
            }
            other => panic!("expected SurfaceDrive frame, got {:?}", other),
        }
    }

    /// [Verifies VC-3.2] An inbound `PeerDeliver` carrying a `DurableEnvelope` whose
    /// payload is a `Drive` maps to `LogicalInput::DriveRequested`, preserving the
    /// surface ops, prompt, envelope, and auth — no flattening to `UserMessage`.
    #[test]
    fn peer_deliver_drive_envelope_maps_to_drive_requested() {
        use crate::agent::world::inputs::DriveCommand;

        let from = "lead-app";
        let durable = DurableEnvelope {
            id: PeerEnvelopeId("env-drive-1".into()),
            payload: PeerPayload::Drive(DriveCommand {
                surface_ops: vec![SurfaceOp::SetValue {
                    surface: 1,
                    element: 2,
                    value: serde_json::json!("typed by peer"),
                    base_version: None,
                }],
                prompt: Some("please continue".into()),
            }),
            auth: Authorization {
                from: PeerId {
                    app_id: from.into(),
                    node_id: "local".into(),
                },
                token: from.into(),
            },
        };
        let payload = serde_json::to_value(&durable).expect("serialise durable envelope");

        match bridge_peer_deliver(from, payload) {
            LogicalInput::DriveRequested {
                from: f,
                envelope,
                drive,
                auth,
            } => {
                assert_eq!(
                    f,
                    PeerId {
                        app_id: from.into(),
                        node_id: "local".into()
                    },
                    "from is rebuilt as the local-node sender PeerId"
                );
                assert_eq!(envelope, PeerEnvelopeId("env-drive-1".into()));
                assert_eq!(drive.surface_ops.len(), 1, "the drive ops are preserved");
                assert_eq!(drive.prompt.as_deref(), Some("please continue"));
                assert_eq!(auth.token, from, "the auth is preserved");
                assert_eq!(auth.from, f, "auth.from matches the sender (authorization passes)");
            }
            other => panic!("expected DriveRequested, got {other:?}"),
        }
    }

    /// [Verifies VC-3.2] A `DurableEnvelope` whose payload is a `Message` maps to
    /// `LogicalInput::PeerDelivered` (a generic peer message), not a `UserMessage`.
    #[test]
    fn peer_deliver_message_envelope_maps_to_peer_delivered() {
        let from = "peer-x";
        let durable = DurableEnvelope {
            id: PeerEnvelopeId("env-msg-1".into()),
            payload: PeerPayload::Message(serde_json::json!({ "text": "hi" })),
            auth: Authorization {
                from: PeerId {
                    app_id: from.into(),
                    node_id: "local".into(),
                },
                token: from.into(),
            },
        };
        let payload = serde_json::to_value(&durable).expect("serialise durable envelope");

        match bridge_peer_deliver(from, payload) {
            LogicalInput::PeerDelivered {
                from: f,
                envelope,
                payload,
            } => {
                assert_eq!(f.app_id, "peer-x");
                assert_eq!(envelope, PeerEnvelopeId("env-msg-1".into()));
                assert_eq!(payload["text"], "hi");
            }
            other => panic!("expected PeerDelivered, got {other:?}"),
        }
    }

    /// A legacy/plain `PeerDeliver` payload (no durable envelope) still maps to a
    /// `PeerDelivered` carrying the raw payload — never a `UserMessage` and never a
    /// decode error.
    #[test]
    fn peer_deliver_legacy_payload_maps_to_peer_delivered_raw() {
        let payload = serde_json::json!({ "text": "legacy" });
        match bridge_peer_deliver("old-app", payload) {
            LogicalInput::PeerDelivered { from, payload, .. } => {
                assert_eq!(from.app_id, "old-app");
                assert_eq!(payload["text"], "legacy");
            }
            other => panic!("expected PeerDelivered, got {other:?}"),
        }
    }

    /// A non-surface-drive `RunTool` (e.g. a plain tool call) returns `None`.
    #[test]
    fn run_tool_non_surface_drive_returns_none() {
        let command = Command::RunTool {
            cmd: 11,
            entity: 0,
            tool: "get_weather".into(),
            args: serde_json::json!({ "city": "Tokyo" }),
            key: CommandKey,
        };
        assert!(
            surface_drive_frame(&command).is_none(),
            "a non-set_value RunTool must not produce a SurfaceDrive"
        );
    }
}
