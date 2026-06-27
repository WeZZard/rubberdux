//! The subprocess-backed [`AppSupervisor`]: one local `rubberduxd --agent`
//! child process per active App, crash-isolated and auto-restarted.
//!
//! [`LocalSupervisor`] is the out-of-process realization of the
//! [`AppSupervisor`](crate::app::supervisor::AppSupervisor) trait. Where
//! `MemorySupervisor` runs each App's `AgentLoop` as an in-process tokio task,
//! `LocalSupervisor` spawns it as a separate OS process — the native worker in
//! `crate::app::runtime::worker` — and bridges that worker's RPC duplex
//! (`crate::protocol`) into the same per-App broadcast surface the board renders
//! from. The two are interchangeable behind the trait; the gateway never learns
//! which one it holds.
//!
//! It is the host-side counterpart of `crate::app::runtime::worker`: that module
//! is the in-child worker (connect, `Hello`, run the loop, stream entries); this
//! module is the host that binds the listener, spawns the child via
//! `std::env::current_exe()`, accepts the worker's `Hello`-routed socket, pumps
//! `EntryNotification` frames into a per-App broadcast, and forwards
//! `send_message` as a `HostToAgent::UserMessage`. A crashed child is restarted
//! with backoff while the host process stays up. The lifecycle is documented in
//! `docs/app/runtime/worker-lifecycle.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::app::registry::store::AppStore;
use crate::app::runtime::lifecycle::{AppLifecycle, ResumeState, idle_window};
use crate::app::runtime::worker_handle::WorkerHandle;
use crate::app::supervisor::{AppSupervisor, BoardEvent, CreateAppRequest};
use crate::app::{App, AppId, AppStatus, BoardPosition};
use crate::agent::interaction::{AgentInteraction, InteractionResponse};
use crate::agent::runtime::port::EntryNotification;
use crate::error::Error;
use crate::app::peer::PeerId;
use crate::app::peer::mailbox::Mailbox;
use crate::host::{
    AcceptedWorker, PeerBroker, PeerRouteOutcome, SurfaceRouter, WorkerStream, accept_worker,
};
use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::trajectory::TrajectoryEvent;

/// Capacity of the per-App entry and trajectory broadcast channels and the board
/// channel. Mirrors `crate::app::supervisor`'s `CHANNEL_CAPACITY`.
const CHANNEL_CAPACITY: usize = 256;

/// Bound on the outbound (host → worker) frame queue per App. Frames are drained
/// promptly by the supervision task, so a small buffer absorbs bursts without
/// unbounded growth.
const OUTBOUND_CAPACITY: usize = 64;

/// Bound on the per-App surface-relay inbound queue: macOS client observations the
/// [`SurfaceRouter`] routes to this worker before the pump writes them to the
/// worker socket. Surface events arrive at human interaction speed, so a small
/// buffer absorbs bursts without back-pressuring the router.
const SURFACE_RELAY_CAPACITY: usize = 64;

/// Initial restart backoff after a worker crash. Doubles on each consecutive
/// crash up to [`MAX_BACKOFF`], resetting once a worker stays up past
/// [`HEALTHY_UPTIME`]. Keeps a crash loop from spinning the host's CPU while
/// still recovering quickly from a transient failure.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// Ceiling for the exponential restart backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How long a worker must run before its crash backoff resets to
/// [`INITIAL_BACKOFF`]. A worker that survives this long is treated as healthy,
/// so an unrelated later crash starts its backoff fresh.
const HEALTHY_UPTIME: Duration = Duration::from_secs(10);

/// How long `start_worker`/`ensure_active` waits for a freshly spawned worker to
/// dial back and identify itself (its `Hello` is accepted and the host-side RPC
/// writer is live) before returning. This makes restore a deterministic barrier:
/// a `send_message` that follows a `restore`/`ensure_active` is delivered to a
/// worker that is already connected and pumping, eliminating the
/// send-after-restore race. The frame queue is buffered regardless, so exceeding
/// this bound only logs a warning and proceeds — the buffered frame still flushes
/// once the worker connects. See `docs/app/runtime/worker-lifecycle.md`.
const CONNECT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `suspend` lets a worker drain after a `HostToAgent::Shutdown` before
/// killing it. The cancellation token always kills the child eventually; this is
/// the grace period in which a cooperative worker can exit on its own after
/// flushing. Kept short so suspend stays responsive.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How often the idle sweeper wakes to tombstone quiet Apps. Independent of the
/// idle window itself (`RUBBERDUX_APP_IDLE_SECS`): the sweep cadence bounds how
/// long past the window an idle App may linger before eviction.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// The mailbox an accepted worker socket is delivered into, keyed by App id.
/// The single accept router (one per supervisor) classifies each incoming
/// `Hello`-routed socket and hands it to the matching App's supervision task,
/// which is blocked waiting for its (re)connection.
type ConnectionInbox = mpsc::Sender<WorkerStream>;

/// Subprocess-backed [`AppSupervisor`]. Owns a loopback RPC listener, an accept
/// router that fans `Hello`-routed sockets to per-App supervision tasks, the
/// live [`WorkerHandle`]s for `Active` Apps, and the board event channel.
pub struct LocalSupervisor {
    store: Arc<dyn AppStore>,
    /// The address worker children dial back on. Bound to `127.0.0.1:0` so the
    /// OS assigns a free port; children are told this exact address.
    rpc_addr: std::net::SocketAddr,
    /// Live worker handles for `Active` Apps, keyed by id. Guarded by an async
    /// mutex so the trait's `&self` methods mutate it without a `&mut self`, and
    /// shared (`Arc`) with the idle sweeper so it can evict quiet workers.
    runtimes: Arc<Mutex<HashMap<AppId, WorkerHandle>>>,
    /// Per-App lifecycle records the idle sweeper reads to decide eviction and
    /// suspend/restore update. An entry exists for every App the supervisor has
    /// ever made `Active`; it persists across a tombstone so the sweeper sees the
    /// `Tombstoned` phase (and skips it). See `src/app/runtime/lifecycle.rs`.
    lifecycles: Arc<Mutex<HashMap<AppId, AppLifecycle>>>,
    /// Per-App connection mailboxes the accept router delivers sockets into.
    /// Shared with the router task so it can route by the `Hello` frame's id.
    inboxes: Arc<Mutex<HashMap<AppId, ConnectionInbox>>>,
    /// Board lifecycle event fan-out, kept alive for the supervisor's lifetime.
    board_tx: broadcast::Sender<BoardEvent>,
    /// The peer broker: the directory + relay that resolves a `PeerSend` by
    /// `PeerId` and forwards opaque envelopes (a switch, not an orchestrator). The
    /// supervisor registers each active worker's delivery sink here, hands the
    /// pump frames to it, and wakes an offline target after the broker queues to
    /// its inbox. See `docs/app/peer/decentralized-messaging.md`.
    peer_broker: Arc<PeerBroker>,
    /// A self-reference handed to each per-App supervision pump so the pump can
    /// call back into the supervisor's restore path (`ensure_active`) when the
    /// broker reports a `PeerSend` was `Queued` to an offline target's inbox.
    /// Empty (a dangling `Weak`) for a supervisor built with `bind`; populated
    /// when built with `bind_shared`, which the host uses. A pump whose upgrade
    /// fails (no live `Arc<Self>`) skips the wake and the message drains on the
    /// target's next restore. See `docs/app/peer/decentralized-messaging.md`.
    me: Weak<LocalSupervisor>,
    /// The host's surface router, shared so each App's RPC pump can register the
    /// worker's inbound surface seam (`register_worker`) and route the worker's
    /// outbound `AgentToHost::SurfaceDrive` frames to the registered macOS client
    /// (`route_drive`), keyed by App id. `None` for a supervisor built with `bind`
    /// (tests with no live surface client); `Some` for the host's `bind_shared`.
    /// See `docs/agent/world/ecs-runtime.md` (Theme 2a/2b; VC-P.1).
    surface_router: Option<Arc<SurfaceRouter>>,
}

/// The async-bound parts of a [`LocalSupervisor`], produced by `assemble` before
/// the choice of self-reference (`Weak<LocalSupervisor>`) is known. Exists so the
/// async listener/router setup happens once and is then finished into a
/// supervisor either by value (`bind`) or inside `Arc::new_cyclic` (`bind_shared`).
struct SupervisorParts {
    store: Arc<dyn AppStore>,
    rpc_addr: std::net::SocketAddr,
    inboxes: Arc<Mutex<HashMap<AppId, ConnectionInbox>>>,
    board_tx: broadcast::Sender<BoardEvent>,
    peer_broker: Arc<PeerBroker>,
}

impl SupervisorParts {
    /// Finish the assembled parts into a supervisor with the given self-reference
    /// and surface router. `me` is empty for `bind` and the live `Weak` for
    /// `bind_shared`; `surface_router` is `None` for `bind` and the host's shared
    /// router for `bind_shared`.
    fn into_supervisor(
        self,
        me: Weak<LocalSupervisor>,
        surface_router: Option<Arc<SurfaceRouter>>,
    ) -> LocalSupervisor {
        LocalSupervisor {
            store: self.store,
            rpc_addr: self.rpc_addr,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            lifecycles: Arc::new(Mutex::new(HashMap::new())),
            inboxes: self.inboxes,
            board_tx: self.board_tx,
            peer_broker: self.peer_broker,
            me,
            surface_router,
        }
    }
}

impl LocalSupervisor {
    /// Bind the loopback RPC listener and start the accept router, returning a
    /// ready supervisor. The router lives for the process lifetime, routing every
    /// worker `Hello` to the matching App's supervision task.
    pub async fn bind(store: Arc<dyn AppStore>) -> Result<Self, Error> {
        // No self-reference: a supervisor moved by value cannot hand its pumps a
        // live `Arc<Self>`, so the peer-send wake degrades to a no-op (the queued
        // message drains on the target's next restore). The host uses
        // `bind_shared` to get the active wake.
        let parts = Self::assemble(store).await?;
        let supervisor = parts.into_supervisor(Weak::new(), None);
        supervisor.spawn_idle_sweeper();
        Ok(supervisor)
    }

    /// Bind a supervisor already wrapped in an `Arc`, with each supervision pump
    /// holding a live `Weak<Self>` so a `PeerSend` queued to an offline target
    /// wakes that target through `ensure_active`. The host uses this so the
    /// peer-send wake is active; `bind` is the by-value variant for callers that
    /// do not need the wake. The `surface_router` is shared so each App's RPC pump
    /// relays surface frames to/from the registered macOS client (Theme 2a/2b).
    /// See `docs/app/peer/decentralized-messaging.md` and
    /// `docs/agent/world/ecs-runtime.md`.
    pub async fn bind_shared(
        store: Arc<dyn AppStore>,
        surface_router: Arc<SurfaceRouter>,
    ) -> Result<Arc<Self>, Error> {
        // `assemble` does the async listener/router setup; the `Arc` is then
        // built cyclically so every pump captures the supervisor's own `Weak` and
        // can call back into `ensure_active`.
        let parts = Self::assemble(store).await?;
        let shared =
            Arc::new_cyclic(|me| parts.into_supervisor(me.clone(), Some(surface_router)));
        shared.spawn_idle_sweeper();
        Ok(shared)
    }

    /// Bind the listener, start the accept router, and create the broker and board
    /// channel — the async setup shared by `bind` and `bind_shared`. The returned
    /// parts are finished into a `LocalSupervisor` with a chosen self-reference.
    async fn assemble(store: Arc<dyn AppStore>) -> Result<SupervisorParts, Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let rpc_addr = listener.local_addr()?;
        let (board_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let inboxes: Arc<Mutex<HashMap<AppId, ConnectionInbox>>> =
            Arc::new(Mutex::new(HashMap::new()));

        Self::spawn_accept_router(listener, inboxes.clone());

        let peer_broker = Arc::new(PeerBroker::new(store.clone()));
        Ok(SupervisorParts {
            store,
            rpc_addr,
            inboxes,
            board_tx,
            peer_broker,
        })
    }

    /// The address worker children connect back on, e.g. for tests that spawn a
    /// child by hand.
    pub fn rpc_addr(&self) -> std::net::SocketAddr {
        self.rpc_addr
    }

    /// Run the single accept loop for this supervisor. Each accepted worker
    /// socket is classified by its first frame; an `App` socket is delivered to
    /// the matching id's inbox so its supervision task picks up the (re)connect.
    /// Sockets without a waiting inbox (a late or stray worker) are dropped.
    fn spawn_accept_router(
        listener: TcpListener,
        inboxes: Arc<Mutex<HashMap<AppId, ConnectionInbox>>>,
    ) {
        tokio::spawn(async move {
            loop {
                match accept_worker(&listener).await {
                    Ok(AcceptedWorker::App { app_id, stream }) => {
                        let id = AppId(app_id);
                        let inbox = inboxes.lock().await.get(&id).cloned();
                        match inbox {
                            Some(tx) => {
                                if tx.send(stream).await.is_err() {
                                    log::warn!(
                                        "App `{id}` worker connected but its supervision task is gone; dropping socket"
                                    );
                                }
                            }
                            None => {
                                log::warn!(
                                    "App `{id}` worker connected with no waiting supervision task; dropping socket"
                                );
                            }
                        }
                    }
                    Ok(AcceptedWorker::VmChild { .. }) => {
                        // A VM child opened on the App listener: this supervisor
                        // only owns native App workers, so close it.
                        log::warn!(
                            "VM child connected to LocalSupervisor listener; closing (no VM path here)"
                        );
                    }
                    Err(e) => {
                        log::error!("LocalSupervisor accept loop error: {e}");
                    }
                }
            }
        });
    }

    /// Start supervising a worker for `app`: install its connection inbox, spawn
    /// the supervision task (which owns the child process and the RPC pump and
    /// restarts a crash with backoff), and return the handle the supervisor
    /// keeps. `app_session_dir` roots the worker's `SessionManager` under the
    /// App's directory.
    /// A restored worker's pump can wake a peer via `ensure_active`, which calls
    /// back into `start_worker`. Returning a boxed (named) future instead of an
    /// `async fn`'s opaque future breaks that otherwise-cyclic type inference:
    /// the concrete `Pin<Box<dyn Future>>` is the cut point in the cycle.
    fn start_worker<'a>(
        &'a self,
        app: &'a App,
        app_session_dir: PathBuf,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WorkerHandle> + Send + 'a>> {
        Box::pin(async move {
            let (entry_tx, _) = broadcast::channel::<EntryNotification>(CHANNEL_CAPACITY);
            let (trajectory_tx, _) = broadcast::channel::<TrajectoryEvent>(CHANNEL_CAPACITY);
            let (outbound_tx, outbound_rx) = mpsc::channel::<HostToAgent>(OUTBOUND_CAPACITY);
            let (conn_tx, conn_rx) = mpsc::channel::<WorkerStream>(1);
            // Fires once, the moment the supervision task has accepted the worker's
            // `Hello` and its outbound RPC writer is live. Awaiting it below turns
            // `start_worker` (and therefore `ensure_active`/`restore`) into a
            // connection-ready barrier so a following `send_message` is delivered to
            // a connected, pumping worker rather than racing a not-yet-connected one.
            let (ready_tx, ready_rx) = oneshot::channel::<()>();
            let cancel = CancellationToken::new();

            // Keep a clone of this connection's sender so the supervision task can
            // tell, on teardown, whether the inbox map still holds *its own* inbox.
            // A `restore` that races a tombstone's grace period reinstalls a fresh
            // inbox under the same id while the old task is still winding down;
            // without this guard the old task's unconditional removal would clobber
            // the new worker's route and strand its `Hello` (see
            // `docs/app/runtime/worker-lifecycle.md`).
            let own_inbox = conn_tx.clone();
            self.inboxes
                .lock()
                .await
                .insert(app.id.clone(), conn_tx);

            // Make this App reachable on the peer network and directly deliverable:
            // register a clone of its outbound sink with the broker. The broker uses
            // it to relay a `PeerDeliver` to this live worker.
            self.peer_broker
                .register(PeerId::local(app.id.clone()), outbound_tx.clone())
                .await;

            let task = SupervisionTask {
                app_id: app.id.clone(),
                rpc_addr: self.rpc_addr,
                app_session_dir,
                entry_tx: entry_tx.clone(),
                cancel: cancel.clone(),
                inboxes: self.inboxes.clone(),
                own_inbox,
                runtimes: self.runtimes.clone(),
                lifecycles: self.lifecycles.clone(),
                peer_broker: self.peer_broker.clone(),
                supervisor: self.me.clone(),
                surface_router: self.surface_router.clone(),
            };
            tokio::spawn(task.run(conn_rx, outbound_rx, Some(ready_tx)));

            // Wait for the worker to connect before returning, bounded so a worker
            // that never dials back does not wedge the caller. The outbound queue is
            // buffered, so a timeout only degrades determinism (the buffered frame
            // still flushes on connect); it does not drop messages.
            match tokio::time::timeout(CONNECT_READY_TIMEOUT, ready_rx).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => log::warn!(
                    "[app-worker:{}] supervision task ended before signaling connection-ready",
                    app.id
                ),
                Err(_) => log::warn!(
                    "[app-worker:{}] worker did not connect within {:?}; proceeding (outbound frames stay buffered)",
                    app.id,
                    CONNECT_READY_TIMEOUT
                ),
            }

            WorkerHandle::new(outbound_tx, entry_tx, trajectory_tx, cancel)
        })
    }

    /// Look up the live App from the store, erroring if it does not exist.
    async fn require_app(&self, id: &AppId) -> Result<App, Error> {
        self.store
            .get(id)?
            .ok_or_else(|| Error::App(format!("App `{id}` does not exist")))
    }

    /// The on-disk home directory the worker roots its session data under, taken
    /// from the store so all of an App's data lives inside the App's directory.
    fn app_session_dir(&self, id: &AppId) -> PathBuf {
        self.store.app_dir(id)
    }

    /// Announce a status change on the board channel.
    fn announce_status(&self, id: &AppId, status: AppStatus) {
        let _ = self.board_tx.send(BoardEvent::StatusChanged {
            id: id.clone(),
            status,
        });
    }

    /// Mark a turn as started on an `Active` App, called right after an outbound
    /// user message is queued to the worker. Blocks the idle sweeper from evicting
    /// the App until the worker's final entry calls `turn_finished`. A no-op for an
    /// App with no lifecycle record.
    async fn mark_turn_started(&self, id: &AppId) {
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(state) = lifecycles.remove(id) {
            lifecycles.insert(id.clone(), state.turn_started(std::time::Instant::now()));
        }
    }

    /// Refresh an `Active` App's lifecycle pending-interaction flag from the live
    /// truth on its [`WorkerHandle`]. Called after answering an interaction so the
    /// sweeper sees the App unblocked once its last interaction is cleared.
    async fn sync_pending_interaction(&self, id: &AppId) {
        let has_pending = {
            let runtimes = self.runtimes.lock().await;
            runtimes
                .get(id)
                .map(|handle| handle.has_pending_interaction())
                .unwrap_or(false)
        };
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(state) = lifecycles.remove(id) {
            lifecycles.insert(id.clone(), state.with_pending_interaction(has_pending));
        }
    }

    /// If `id` is not currently `Active`, restore its worker from disk
    /// transparently: the conversation history lives in the App's `session.jsonl`
    /// (continuously persisted by the worker), and any `resume.json` carries the
    /// interactions awaiting answers. Returns after the worker is started so the
    /// caller's message/subscription proceeds against a live worker. A no-op for
    /// an already-`Active` App.
    ///
    /// This is the transparent half of tombstoning: every message-path method
    /// calls it first so a caller never observes the `Tombstoned` gap.
    async fn ensure_active(&self, id: &AppId) -> Result<(), Error> {
        if self.runtimes.lock().await.contains_key(id) {
            return Ok(());
        }
        let app = self.require_app(id).await?;
        let session_dir = self.app_session_dir(id);

        // The worker reads its history from `session.jsonl` under the session
        // dir; `resume.json` carries the transient state. Load it and re-attach
        // the pending interactions to the new handle *before* clearing the record,
        // so an interaction the worker was awaiting an answer for survives the
        // suspend→restore gap. (Peer-message redelivery is a later task.)
        let resume = ResumeState::load(&session_dir);
        let mut handle = self.start_worker(&app, session_dir.clone()).await;
        let restored_pending = resume.pending_interactions;
        let has_pending = !restored_pending.is_empty();
        handle.set_pending_interactions(restored_pending);
        ResumeState::clear(&session_dir)?;

        self.runtimes.lock().await.insert(id.clone(), handle);
        // A just-restored App starts a fresh idle window; carry forward the
        // pending-interaction block so the sweeper does not immediately re-evict
        // an App that is still awaiting a user answer.
        self.lifecycles.lock().await.insert(
            id.clone(),
            AppLifecycle::active(std::time::Instant::now())
                .with_pending_interaction(has_pending),
        );
        self.announce_status(id, AppStatus::Active);

        // Drain any peer messages queued to this App while it was offline. A
        // tombstoned or human-archived App is still addressable: the broker
        // queued envelopes to its `inbox.jsonl`, and restore delivers them in
        // arrival order as `PeerDeliver` frames so the worker reacts to them.
        // See docs/app/peer/decentralized-messaging.md.
        self.drain_peer_inbox(id).await;
        Ok(())
    }

    /// Deliver every peer message queued to `id`'s inbox to its now-live worker,
    /// in arrival order, then clear the inbox. A no-op when the inbox is empty.
    /// Called on restore so an offline target's messages are not lost.
    ///
    /// The inbox is read from the App's *addressable home* (live or archive), the
    /// same directory the broker queued into. An archived App's inbox lives under
    /// the archive, so draining the live session directory alone would strand it.
    /// See `docs/app/peer/decentralized-messaging.md`.
    async fn drain_peer_inbox(&self, id: &AppId) {
        let home = self.store.addressable_home(id);
        let mailbox = Mailbox::in_dir(&home);
        let envelopes = match mailbox.drain() {
            Ok(envelopes) => envelopes,
            Err(e) => {
                log::warn!("failed to drain peer inbox for App `{id}`: {e}");
                return;
            }
        };
        if envelopes.is_empty() {
            return;
        }
        let runtimes = self.runtimes.lock().await;
        let Some(handle) = runtimes.get(id) else {
            return;
        };
        for envelope in envelopes {
            let frame = HostToAgent::PeerDeliver {
                from: envelope.from.app_id.to_string(),
                payload: envelope.payload,
            };
            if let Err(e) = handle.send_frame(frame).await {
                log::warn!("failed to deliver queued peer message to App `{id}`: {e}");
                break;
            }
        }
    }

    /// Start the background idle sweeper: a tokio task that periodically
    /// tombstones every `Active` App that has been quiet past the configured idle
    /// window with no in-flight turn or pending interaction. Honors
    /// `RUBBERDUX_APP_IDLE_SECS` (default 300). Runs for the supervisor's lifetime
    /// and never blocks a request path. See `src/app/runtime/lifecycle.rs`.
    fn spawn_idle_sweeper(&self) {
        let runtimes = self.runtimes.clone();
        let lifecycles = self.lifecycles.clone();
        let inboxes = self.inboxes.clone();
        let store = self.store.clone();
        let board_tx = self.board_tx.clone();
        let peer_broker = self.peer_broker.clone();
        tokio::spawn(async move {
            let window = idle_window();
            let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                ticker.tick().await;
                let now = std::time::Instant::now();
                let evictable: Vec<AppId> = {
                    let lifecycles = lifecycles.lock().await;
                    lifecycles
                        .iter()
                        .filter(|(_, state)| state.is_idle_evictable(now, window))
                        .map(|(id, _)| id.clone())
                        .collect()
                };
                for id in evictable {
                    if let Err(e) = tombstone_app(
                        &id,
                        &runtimes,
                        &lifecycles,
                        &inboxes,
                        store.as_ref(),
                        &board_tx,
                        &peer_broker,
                    )
                    .await
                    {
                        log::warn!("idle sweeper failed to tombstone App `{id}`: {e}");
                    } else {
                        log::info!("idle sweeper tombstoned App `{id}`");
                    }
                }
            }
        });
    }
}

/// Tombstone one App: persist its `resume.json`, ask the worker to shut down
/// cooperatively, wait a short grace period, then cancel the handle (which kills
/// the child and tears down the supervision task) and mark the App `Tombstoned`.
/// Shared by `LocalSupervisor::suspend` and the idle sweeper so both follow the
/// same persist→shutdown→kill sequence. A no-op for an App with no live worker.
async fn tombstone_app(
    id: &AppId,
    runtimes: &Mutex<HashMap<AppId, WorkerHandle>>,
    lifecycles: &Mutex<HashMap<AppId, AppLifecycle>>,
    inboxes: &Mutex<HashMap<AppId, ConnectionInbox>>,
    store: &dyn AppStore,
    board_tx: &broadcast::Sender<BoardEvent>,
    peer_broker: &PeerBroker,
) -> Result<(), Error> {
    let removed = runtimes.lock().await.remove(id);
    let Some(handle) = removed else {
        return Ok(());
    };

    // Drop this worker's live delivery sink from the broker so a later `PeerSend`
    // to a now-offline App is queued to its inbox rather than written to a dead
    // channel. The App stays addressable — archiving/tombstoning never removes it
    // from the peer network; it only changes the delivery path to inbox-queue.
    peer_broker.unregister(&PeerId::local(id.clone())).await;

    // Persist the transient resume state before the worker exits. History is
    // already durable in `session.jsonl`; this captures only the pending
    // interactions plus the documented (currently empty) peer-message slot.
    let resume = ResumeState {
        pending_interactions: handle.pending_interactions(),
        undelivered_peer_messages: Vec::new(),
    };
    resume.persist(&store.app_dir(id))?;

    // Ask the worker to shut down cooperatively; ignore a send failure, which
    // just means it is already gone.
    let _ = handle.send_frame(HostToAgent::Shutdown).await;
    tokio::time::sleep(SHUTDOWN_GRACE).await;

    // The cancellation token kills the child and exits the supervision task,
    // which tears down the App's connection inbox on its way out.
    handle.shutdown();

    // The connection inbox is torn down by the cancelled supervision task; it is
    // referenced here only to keep the routine's shared-state signature explicit.
    let _ = inboxes;

    {
        let mut lifecycles = lifecycles.lock().await;
        if let Some(state) = lifecycles.remove(id) {
            lifecycles.insert(id.clone(), state.tombstoned());
        }
    }

    let _ = board_tx.send(BoardEvent::StatusChanged {
        id: id.clone(),
        status: AppStatus::Tombstoned,
    });
    Ok(())
}

/// The owned state of one App's supervision task: it spawns the child, awaits its
/// connection on the inbox, pumps the RPC duplex, and restarts a crash with
/// backoff until cancelled.
struct SupervisionTask {
    app_id: AppId,
    rpc_addr: std::net::SocketAddr,
    app_session_dir: PathBuf,
    entry_tx: broadcast::Sender<EntryNotification>,
    cancel: CancellationToken,
    inboxes: Arc<Mutex<HashMap<AppId, ConnectionInbox>>>,
    /// This task's own connection inbox sender, kept solely to identify it on
    /// teardown: the task removes the App's inbox only when the map still holds
    /// *this* sender (`Sender::same_channel`). A `restore` that races this task's
    /// shutdown reinstalls a fresh inbox under the same id; comparing identity
    /// stops this task from clobbering the new worker's route.
    own_inbox: ConnectionInbox,
    /// Shared with the supervisor so the RPC pump can record a worker-raised
    /// interaction on this App's [`WorkerHandle`] (a `Tombstoned` suspend later
    /// captures it in `resume.json`). See `docs/app/runtime/worker-lifecycle.md`.
    runtimes: Arc<Mutex<HashMap<AppId, WorkerHandle>>>,
    /// Shared with the supervisor so the RPC pump can update this App's lifecycle
    /// activity flags from real worker events: a final entry finishes the turn,
    /// and an interaction frame sets the pending-interaction block. The idle
    /// sweeper reads these to decide eviction.
    lifecycles: Arc<Mutex<HashMap<AppId, AppLifecycle>>>,
    /// The peer broker the pump relays this worker's `PeerSend`/`PeerList` frames
    /// through. The pump hands the broker the envelope and writes the broker's
    /// `PeerListResult` back; the broker makes the routing decision (deliver vs.
    /// queue), keeping the pump a thin conduit.
    peer_broker: Arc<PeerBroker>,
    /// A `Weak` back to the supervisor that owns this pump, so a `PeerSend` the
    /// broker reports as `Queued` to an offline target can wake that target
    /// through the supervisor's `ensure_active` restore path. Empty when the
    /// supervisor was built by value (`bind`), in which case the wake is skipped
    /// and the message drains on the target's next restore.
    supervisor: Weak<LocalSupervisor>,
    /// The host's surface router, so the pump can register this worker's inbound
    /// surface seam and route its outbound `SurfaceDrive` frames to the registered
    /// macOS client, keyed by App id. `None` when no live surface relay is wired
    /// (the `bind` path used by tests). See `docs/agent/world/ecs-runtime.md`.
    surface_router: Option<Arc<SurfaceRouter>>,
}

impl SupervisionTask {
    /// Supervise the worker across restarts. Each iteration spawns a child, waits
    /// for it to connect, pumps until it disconnects or crashes, then backs off
    /// before respawning. Exits when the handle is dropped (cancel fires),
    /// removing the App's connection inbox on the way out.
    async fn run(
        self,
        mut conn_rx: mpsc::Receiver<WorkerStream>,
        mut outbound_rx: mpsc::Receiver<HostToAgent>,
        mut ready_tx: Option<oneshot::Sender<()>>,
    ) {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            let started = std::time::Instant::now();
            let outcome = self
                .spawn_and_pump(&mut conn_rx, &mut outbound_rx, &mut ready_tx)
                .await;

            if self.cancel.is_cancelled() {
                break;
            }

            match outcome {
                Ok(()) => {
                    log::info!("[app-worker:{}] worker exited cleanly", self.app_id);
                }
                Err(e) => {
                    log::warn!("[app-worker:{}] worker failed: {e}", self.app_id);
                }
            }

            // A worker that ran long enough is treated as healthy: reset the
            // backoff so an unrelated later crash recovers quickly.
            if started.elapsed() >= HEALTHY_UPTIME {
                backoff = INITIAL_BACKOFF;
            }

            log::info!(
                "[app-worker:{}] restarting in {:?}",
                self.app_id,
                backoff
            );
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }

        // Tear down the connection inbox so the accept router stops routing to a
        // dead task — but only if the map still holds *this* task's inbox. A
        // `restore` racing this shutdown may have already reinstalled a fresh inbox
        // under the same id; removing it would strand the new worker's `Hello` and
        // wedge its connect barrier. Compare channel identity so only the owning
        // task removes its own entry. See `docs/app/runtime/worker-lifecycle.md`.
        {
            let mut inboxes = self.inboxes.lock().await;
            if inboxes
                .get(&self.app_id)
                .is_some_and(|current| current.same_channel(&self.own_inbox))
            {
                inboxes.remove(&self.app_id);
            }
        }
        log::info!("[app-worker:{}] supervision stopped", self.app_id);
    }

    /// Spawn one child process and pump its RPC duplex until it disconnects, the
    /// task is cancelled, or an error occurs. The child is killed when this
    /// returns so a leaked process never outlives its supervision iteration.
    async fn spawn_and_pump(
        &self,
        conn_rx: &mut mpsc::Receiver<WorkerStream>,
        outbound_rx: &mut mpsc::Receiver<HostToAgent>,
        ready_tx: &mut Option<oneshot::Sender<()>>,
    ) -> Result<(), Error> {
        let exe = std::env::current_exe()
            .map_err(|e| Error::App(format!("locate current executable: {e}")))?;
        let mut child = Command::new(exe)
            .arg("--agent")
            .arg("--rpc-host")
            .arg(self.rpc_addr.to_string())
            .arg("--task-id")
            .arg(self.app_id.as_str())
            .arg("--app-session-dir")
            .arg(&self.app_session_dir)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::App(format!("spawn worker for App `{}`: {e}", self.app_id)))?;

        // Wait for the child to dial back and identify itself via `Hello`. Give
        // up if the task is cancelled or the child exits before connecting.
        let stream = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                let _ = child.kill().await;
                return Ok(());
            }
            status = child.wait() => {
                return Err(Error::App(format!(
                    "worker for App `{}` exited before connecting: {status:?}",
                    self.app_id
                )));
            }
            stream = conn_rx.recv() => match stream {
                Some(stream) => stream,
                None => {
                    let _ = child.kill().await;
                    return Ok(());
                }
            }
        };

        // The worker's `Hello` was accepted by the router and its socket handed to
        // us, so the outbound RPC writer (`stream.writer`) is live. Signal
        // connection-ready exactly once — the first connect, not crash-restarts —
        // so `start_worker`/`ensure_active` can stop waiting and the following
        // `send_message` reaches a connected, pumping worker.
        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(());
        }

        let result = self.pump(stream, &mut child, outbound_rx).await;
        // Always reap the child so a crashed-but-not-exited or still-running
        // process is killed rather than leaked.
        let _ = child.kill().await;
        result
    }

    /// Mark this App's in-flight turn as finished in its lifecycle record,
    /// refreshing the idle clock so the sweeper measures the window from the end
    /// of the work. A no-op if the App has no lifecycle record yet.
    async fn finish_turn(&self) {
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(state) = lifecycles.remove(&self.app_id) {
            lifecycles.insert(
                self.app_id.clone(),
                state.turn_finished(std::time::Instant::now()),
            );
        }
    }

    /// Record a worker-raised interaction on this App's handle and set the
    /// lifecycle pending-interaction block, so the sweeper does not evict an App
    /// awaiting a user answer and a later suspend captures it in `resume.json`.
    async fn capture_interaction(&self, interaction: AgentInteraction) {
        {
            let mut runtimes = self.runtimes.lock().await;
            if let Some(handle) = runtimes.get_mut(&self.app_id) {
                handle.add_interaction(interaction);
            } else {
                // No live handle (a race with teardown): nothing to record.
                return;
            }
        }
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(state) = lifecycles.remove(&self.app_id) {
            lifecycles.insert(
                self.app_id.clone(),
                state.with_pending_interaction(true),
            );
        }
    }

    /// Pump the worker's RPC duplex: forward inbound `EntryNotification` frames to
    /// the per-App broadcast, forward queued outbound frames to the worker, and
    /// return when the worker disconnects, exits, or the task is cancelled.
    async fn pump(
        &self,
        stream: WorkerStream,
        child: &mut tokio::process::Child,
        outbound_rx: &mut mpsc::Receiver<HostToAgent>,
    ) -> Result<(), Error> {
        let WorkerStream {
            mut reader,
            mut writer,
        } = stream;
        log::info!("[app-worker:{}] connected, pumping RPC", self.app_id);

        // Register this worker's inbound surface seam so the host's SurfaceRouter
        // relays a macOS client's observations to it, keyed by App id. The seam
        // carries `AgentToHost::SurfaceObservation`/`SurfaceMutated`; the select
        // arm below converts each to the host→worker `HostToAgent` form and writes
        // it to the worker socket. Re-registration on a crash-restart simply
        // overwrites the stale entry. See `docs/agent/world/ecs-runtime.md`
        // (Theme 2b; VC-P.1).
        let (surface_inbound_tx, mut surface_inbound_rx) =
            mpsc::channel::<AgentToHost>(SURFACE_RELAY_CAPACITY);
        if let Some(router) = self.surface_router.as_ref() {
            router
                .register_worker(self.app_id.to_string(), surface_inbound_tx.clone())
                .await;
        }
        // Hold the original sender so `surface_inbound_rx` stays open for the
        // pump's lifetime even with no router attached (the `bind` test path).
        let _surface_inbound_tx = surface_inbound_tx;

        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),

                // Worker → host: read the next frame and fan it out.
                frame = protocol::read_message::<AgentToHost>(&mut reader) => {
                    match frame? {
                        Some(AgentToHost::EntryNotification { entry, is_final }) => {
                            // The final entry of a turn ends the in-flight-turn
                            // block so the idle sweeper can measure the window from
                            // the end of the work; non-final entries are just
                            // streamed.
                            if is_final {
                                self.finish_turn().await;
                            }
                            // No active subscribers is not an error; keep draining
                            // so the worker's stream does not back up.
                            let _ = self.entry_tx.send(EntryNotification { entry, is_final });
                        }
                        Some(AgentToHost::Interaction { interaction }) => {
                            // The worker is blocked on a user answer: record it on
                            // the handle (so a suspend captures it in `resume.json`)
                            // and set the lifecycle pending flag (so the sweeper
                            // never evicts a blocked App).
                            self.capture_interaction(interaction).await;
                        }
                        Some(AgentToHost::Hello { .. }) => {
                            // The routing `Hello` was already consumed by the
                            // accept router; a second one is unexpected. Ignore.
                        }
                        Some(AgentToHost::PeerSend { to, payload }) => {
                            // Relay through the broker, which resolves the target
                            // and decides deliver-vs-queue. The pump makes no
                            // routing decision of its own (the host is a switch).
                            // `to` is an App id on the local node for now; the
                            // PeerId carries the node so federation needs no change
                            // here. See docs/app/peer/decentralized-messaging.md.
                            let from = PeerId::local(self.app_id.clone());
                            let to_peer = PeerId::local(AppId(to));
                            match self.peer_broker.relay(from, to_peer, payload).await {
                                Ok(PeerRouteOutcome::Delivered) => log::debug!(
                                    "[app-worker:{}] peer_send delivered",
                                    self.app_id
                                ),
                                Ok(PeerRouteOutcome::Queued { wake }) => {
                                    // The target's worker is offline; the broker
                                    // queued the envelope to its inbox. Restore the
                                    // target through the supervisor's own restore
                                    // path (`ensure_active`, which drains the inbox)
                                    // so it picks the message up. The broker owns no
                                    // supervision, so the wake happens here. If the
                                    // owning supervisor is gone, skip the wake and
                                    // let the next restore drain the inbox.
                                    log::debug!(
                                        "[app-worker:{}] peer_send queued, waking {}",
                                        self.app_id,
                                        wake.app_id
                                    );
                                    match self.supervisor.upgrade() {
                                        Some(supervisor) => {
                                            // Run the restore on its own task: the
                                            // restore spawns the target's pump, so
                                            // awaiting it inline would make this
                                            // pump's future recursively reference
                                            // itself. Detaching keeps both pumps
                                            // independent and the pump non-blocking.
                                            let source = self.app_id.clone();
                                            let target = wake.app_id.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) =
                                                    supervisor.ensure_active(&target).await
                                                {
                                                    log::warn!(
                                                        "[app-worker:{source}] peer_send wake of {target} failed: {e}"
                                                    );
                                                }
                                            });
                                        }
                                        None => log::debug!(
                                            "[app-worker:{}] peer_send queued to {} but supervisor is gone; drains on next restore",
                                            self.app_id,
                                            wake.app_id
                                        ),
                                    }
                                }
                                Ok(PeerRouteOutcome::Rejected) => log::debug!(
                                    "[app-worker:{}] peer_send rejected",
                                    self.app_id
                                ),
                                Err(e) => log::warn!(
                                    "[app-worker:{}] peer_send relay failed: {e}",
                                    self.app_id
                                ),
                            }
                        }
                        Some(AgentToHost::PeerList) => {
                            // Answer "who can I talk to right now?" from the
                            // broker's dynamic directory, most-recently-used first.
                            let from = PeerId::local(self.app_id.clone());
                            let peers = self
                                .peer_broker
                                .list_for(&from)
                                .await
                                .into_iter()
                                .map(|id| id.app_id.to_string())
                                .collect();
                            let result = HostToAgent::PeerListResult { peers };
                            protocol::write_message(&mut writer, &result).await?;
                        }
                        Some(AgentToHost::SurfaceDrive { ops, cmd, key }) => {
                            // The worker's World driver ran a `set_value` surface
                            // tool. Repackage the relay frame as the host→client
                            // `HostToAgent::SurfaceDrive` and route it to this App's
                            // registered macOS client (the host is a switch, keyed
                            // by App id). A missing client/router drops it.
                            // See docs/agent/world/ecs-runtime.md (Theme 2a).
                            let drive = HostToAgent::SurfaceDrive { ops, cmd, key };
                            match self.surface_router.as_ref() {
                                Some(router) => {
                                    router.route_drive(self.app_id.as_str(), drive).await;
                                }
                                None => log::debug!(
                                    "[app-worker:{}] SurfaceDrive with no surface router; dropping",
                                    self.app_id
                                ),
                            }
                        }
                        Some(other) => {
                            log::debug!(
                                "[app-worker:{}] unhandled worker frame: {other:?}",
                                self.app_id
                            );
                        }
                        None => {
                            log::info!("[app-worker:{}] worker disconnected", self.app_id);
                            return Ok(());
                        }
                    }
                }

                // Host → worker: deliver a queued outbound frame.
                outbound = outbound_rx.recv() => {
                    match outbound {
                        Some(frame) => protocol::write_message(&mut writer, &frame).await?,
                        // The handle was dropped: nothing more to send, but keep
                        // reading until cancel/disconnect drives the exit.
                        None => return Ok(()),
                    }
                }

                // Surface relay (host → worker): a macOS client observation the
                // router routed to this App. Convert the inbound `AgentToHost`
                // surface frame to its host→worker `HostToAgent` counterpart and
                // write it to the worker; its bridge folds it into the World input
                // queue. See docs/agent/world/ecs-runtime.md (Theme 2b; Inv 18).
                inbound = surface_inbound_rx.recv() => {
                    match inbound {
                        Some(AgentToHost::SurfaceObservation { observed }) => {
                            let frame = HostToAgent::SurfaceObservation { observed };
                            protocol::write_message(&mut writer, &frame).await?;
                        }
                        Some(AgentToHost::SurfaceMutated { op, cause }) => {
                            let frame = HostToAgent::SurfaceMutated { op, cause };
                            protocol::write_message(&mut writer, &frame).await?;
                        }
                        // Non-surface frames never reach this seam (the router only
                        // routes observations here); ignore defensively. `None` is
                        // unreachable while the pump holds `_surface_inbound_tx`.
                        Some(other) => log::debug!(
                            "[app-worker:{}] unexpected frame on surface seam: {other:?}",
                            self.app_id
                        ),
                        None => {}
                    }
                }

                // The child process exited out from under us (crash).
                status = child.wait() => {
                    return Err(Error::App(format!(
                        "worker for App `{}` exited: {status:?}",
                        self.app_id
                    )));
                }
            }
        }
    }
}

impl AppSupervisor for LocalSupervisor {
    async fn create_app(
        &self,
        request: CreateAppRequest,
        initial_prompt: String,
    ) -> Result<App, Error> {
        let mut app = App::new(AppId::now(), request.title, request.icon, request.position);
        // The summary doubles as the worker's system-prompt seed at this seam,
        // mirroring `MemorySupervisor`.
        app.summary = initial_prompt.clone();
        self.store.create(&app)?;

        let session_dir = self.app_session_dir(&app.id);
        let handle = self.start_worker(&app, session_dir).await;

        // Deliver the originating task as the worker's first user turn.
        handle
            .send_frame(HostToAgent::UserMessage {
                text: initial_prompt,
                telegram_message_id: None,
            })
            .await?;

        let mut active = app.clone();
        active.status = AppStatus::Active;
        self.runtimes.lock().await.insert(app.id.clone(), handle);
        self.lifecycles
            .lock()
            .await
            .insert(app.id.clone(), AppLifecycle::active(std::time::Instant::now()));
        // The originating prompt is the App's first turn: mark it in flight so the
        // sweeper does not evict a freshly created App while it is still working.
        self.mark_turn_started(&app.id).await;

        let _ = self.board_tx.send(BoardEvent::Created(active.clone()));
        Ok(active)
    }

    async fn list(&self) -> Result<Vec<App>, Error> {
        let mut apps = self.store.list(false)?;
        let runtimes = self.runtimes.lock().await;
        for app in &mut apps {
            if runtimes.contains_key(&app.id) {
                app.status = AppStatus::Active;
            }
        }
        Ok(apps)
    }

    async fn get(&self, id: &AppId) -> Result<Option<App>, Error> {
        let Some(mut app) = self.store.get(id)? else {
            return Ok(None);
        };
        if self.runtimes.lock().await.contains_key(id) {
            app.status = AppStatus::Active;
        }
        Ok(Some(app))
    }

    async fn send_message(&self, id: &AppId, text: String) -> Result<(), Error> {
        // Transparently restore a tombstoned App before delivering the message,
        // so the caller never observes the suspended gap.
        self.ensure_active(id).await?;
        let runtimes = self.runtimes.lock().await;
        let handle = runtimes
            .get(id)
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))?;
        handle
            .send_frame(HostToAgent::UserMessage {
                text,
                telegram_message_id: None,
            })
            .await?;
        drop(runtimes);
        // An outbound user message starts a turn: block eviction until the
        // worker's final entry finishes it in the pump.
        self.mark_turn_started(id).await;
        Ok(())
    }

    async fn subscribe_entries(
        &self,
        id: &AppId,
    ) -> Result<broadcast::Receiver<EntryNotification>, Error> {
        // Restore first so a subscription on a tombstoned App attaches to a live
        // worker's stream rather than failing.
        self.ensure_active(id).await?;
        let runtimes = self.runtimes.lock().await;
        runtimes
            .get(id)
            .map(|handle| handle.subscribe_entries())
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))
    }

    async fn subscribe_trajectory(
        &self,
        id: &AppId,
    ) -> Result<broadcast::Receiver<TrajectoryEvent>, Error> {
        self.ensure_active(id).await?;
        let runtimes = self.runtimes.lock().await;
        runtimes
            .get(id)
            .map(|handle| handle.subscribe_trajectory())
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))
    }

    async fn pending_interactions(&self, id: &AppId) -> Result<Vec<AgentInteraction>, Error> {
        let runtimes = self.runtimes.lock().await;
        runtimes
            .get(id)
            .map(|handle| handle.pending_interactions())
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))
    }

    async fn respond_to_interaction(
        &self,
        id: &AppId,
        response: InteractionResponse,
    ) -> Result<(), Error> {
        // Answering an interaction on a tombstoned App restores it first so the
        // answer reaches a live worker.
        self.ensure_active(id).await?;
        let mut runtimes = self.runtimes.lock().await;
        let handle = runtimes
            .get_mut(id)
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))?;
        // Route the answer back to the worker, then drop the matching pending
        // interaction. See docs/agent/interaction.md.
        let request_id = response.request_id().to_string();
        handle
            .send_frame(HostToAgent::InteractionAnswer { response })
            .await?;
        handle.clear_interaction(&request_id);
        drop(runtimes);
        // Answering resumes the worker's turn and may clear the last pending
        // interaction: mark the turn in flight and re-sync the pending flag from
        // the handle's live truth so the sweeper sees the App unblocked.
        self.mark_turn_started(id).await;
        self.sync_pending_interaction(id).await;
        Ok(())
    }

    async fn suspend(&self, id: &AppId) -> Result<(), Error> {
        // Suspend follows the same persist→shutdown→kill sequence the idle
        // sweeper uses, so manual and automatic tombstoning are identical.
        tombstone_app(
            id,
            &self.runtimes,
            &self.lifecycles,
            &self.inboxes,
            self.store.as_ref(),
            &self.board_tx,
            &self.peer_broker,
        )
        .await
    }

    async fn restore(&self, id: &AppId) -> Result<(), Error> {
        // `ensure_active` is the restore mechanism: it re-runs the spawn path
        // from the durable session plus `resume.json`. A no-op if already active.
        self.ensure_active(id).await
    }

    async fn archive(&self, id: &AppId) -> Result<(), Error> {
        self.suspend(id).await?;
        self.store.archive(id)?;
        let _ = self.board_tx.send(BoardEvent::Archived(id.clone()));
        Ok(())
    }

    async fn move_app(&self, id: &AppId, position: BoardPosition) -> Result<(), Error> {
        self.require_app(id).await?;
        let _ = self.board_tx.send(BoardEvent::Moved {
            id: id.clone(),
            position,
        });
        Ok(())
    }

    fn subscribe_board(&self) -> broadcast::Receiver<BoardEvent> {
        self.board_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::registry::store::FilesystemAppStore;
    use crate::app::IconSpec;

    fn temp_store() -> (Arc<dyn AppStore>, PathBuf) {
        let home = tempfile::tempdir().unwrap().into_path();
        let apps_dir = home.join("apps");
        let store: Arc<dyn AppStore> =
            Arc::new(FilesystemAppStore::with_apps_dir(apps_dir.clone()));
        (store, apps_dir)
    }

    #[tokio::test]
    async fn bind_assigns_a_loopback_rpc_address() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let addr = supervisor.rpc_addr();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0, "the OS must have assigned a concrete port");
    }

    #[tokio::test]
    async fn subscribe_entries_errors_without_worker() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let missing = AppId("2026-06-10-00-00-00-UTC".into());
        assert!(matches!(
            supervisor.subscribe_entries(&missing).await,
            Err(Error::App(_))
        ));
        assert!(matches!(
            supervisor.send_message(&missing, "x".into()).await,
            Err(Error::App(_))
        ));
    }

    #[tokio::test]
    async fn move_app_emits_board_moved_event() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store.clone()).await.unwrap();

        // Persist an App directly so move has a target without spawning a worker.
        let app = App::new(
            AppId::now(),
            "t".into(),
            IconSpec { symbol: "s".into(), color: "#000000".into() },
            BoardPosition { row: 0, column: 0 },
        );
        store.create(&app).unwrap();

        let mut board = supervisor.subscribe_board();
        let target = BoardPosition { row: 4, column: 9 };
        supervisor.move_app(&app.id, target).await.unwrap();

        match board.recv().await.unwrap() {
            BoardEvent::Moved { id, position } => {
                assert_eq!(id, app.id);
                assert_eq!(position, target);
            }
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_worker_installs_a_connection_inbox() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let app = App::new(
            AppId::now(),
            "t".into(),
            IconSpec { symbol: "s".into(), color: "#000000".into() },
            BoardPosition { row: 0, column: 0 },
        );
        let session_dir = tempfile::tempdir().unwrap().into_path();
        let _handle = supervisor.start_worker(&app, session_dir).await;
        // The supervision task must have registered the App's inbox so the accept
        // router can route its `Hello`-identified socket.
        assert!(supervisor.inboxes.lock().await.contains_key(&app.id));
    }

    #[tokio::test]
    async fn suspend_removes_runtime_and_inbox() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let app = App::new(
            AppId::now(),
            "t".into(),
            IconSpec { symbol: "s".into(), color: "#000000".into() },
            BoardPosition { row: 0, column: 0 },
        );
        store_create(&supervisor, &app);
        let session_dir = tempfile::tempdir().unwrap().into_path();
        let handle = supervisor.start_worker(&app, session_dir).await;
        supervisor
            .runtimes
            .lock()
            .await
            .insert(app.id.clone(), handle);

        supervisor.suspend(&app.id).await.unwrap();
        assert!(!supervisor.runtimes.lock().await.contains_key(&app.id));

        // Give the cancelled supervision task a moment to drop its inbox.
        let removed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !supervisor.inboxes.lock().await.contains_key(&app.id) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false);
        assert!(removed, "suspending must tear down the App's connection inbox");
    }

    /// Persist an App via the supervisor's store so `suspend`'s store-backed paths
    /// have a manifest to read.
    fn store_create(supervisor: &LocalSupervisor, app: &App) {
        supervisor.store.create(app).unwrap();
    }

    #[tokio::test]
    async fn ensure_active_restores_and_consumes_resume_record() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let app = App::new(
            AppId::now(),
            "t".into(),
            IconSpec { symbol: "s".into(), color: "#000000".into() },
            BoardPosition { row: 0, column: 0 },
        );
        store_create(&supervisor, &app);

        // Leave a resume record on disk as a prior tombstone would, so restore
        // has transient state to consume.
        let session_dir = supervisor.app_session_dir(&app.id);
        ResumeState::default().persist(&session_dir).unwrap();
        assert!(ResumeState::path_in(&session_dir).is_file());

        // No live worker yet: ensure_active must restore it transparently.
        assert!(!supervisor.runtimes.lock().await.contains_key(&app.id));
        supervisor.ensure_active(&app.id).await.unwrap();

        // Restore registers a live handle and an Active lifecycle record, and
        // consumes the resume record so a later tombstone writes a fresh one.
        assert!(supervisor.runtimes.lock().await.contains_key(&app.id));
        assert!(supervisor.lifecycles.lock().await.contains_key(&app.id));
        assert!(!ResumeState::path_in(&session_dir).is_file());

        // A second call is a no-op (already active).
        supervisor.ensure_active(&app.id).await.unwrap();
        assert!(supervisor.runtimes.lock().await.contains_key(&app.id));
    }

    #[tokio::test]
    async fn suspend_persists_resume_record() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let app = App::new(
            AppId::now(),
            "t".into(),
            IconSpec { symbol: "s".into(), color: "#000000".into() },
            BoardPosition { row: 0, column: 0 },
        );
        store_create(&supervisor, &app);
        let session_dir = supervisor.app_session_dir(&app.id);
        let handle = supervisor.start_worker(&app, session_dir.clone()).await;
        supervisor.runtimes.lock().await.insert(app.id.clone(), handle);

        supervisor.suspend(&app.id).await.unwrap();

        // Tombstoning wrote a durable resume record for the next restore.
        assert!(ResumeState::path_in(&session_dir).is_file());
        assert!(!supervisor.runtimes.lock().await.contains_key(&app.id));
    }

    #[tokio::test]
    async fn pending_interaction_survives_suspend_then_restore() {
        let (store, _dir) = temp_store();
        let supervisor = LocalSupervisor::bind(store).await.unwrap();
        let app = App::new(
            AppId::now(),
            "t".into(),
            IconSpec { symbol: "s".into(), color: "#000000".into() },
            BoardPosition { row: 0, column: 0 },
        );
        store_create(&supervisor, &app);
        let session_dir = supervisor.app_session_dir(&app.id);

        // Bring the App up and record an interaction the worker is awaiting an
        // answer for, exactly as the RPC pump would on an `Interaction` frame.
        let mut handle = supervisor.start_worker(&app, session_dir.clone()).await;
        handle.add_interaction(AgentInteraction::Approval {
            request_id: "await-me".into(),
            app_id: app.id.to_string(),
            flavor: crate::agent::interaction::ApprovalFlavor::Permission,
            prompt: "proceed?".into(),
        });
        supervisor.runtimes.lock().await.insert(app.id.clone(), handle);
        supervisor
            .lifecycles
            .lock()
            .await
            .insert(
                app.id.clone(),
                AppLifecycle::active(std::time::Instant::now()).with_pending_interaction(true),
            );

        // Suspend persists the pending interaction into `resume.json`...
        supervisor.suspend(&app.id).await.unwrap();
        assert!(!supervisor.runtimes.lock().await.contains_key(&app.id));
        let persisted = ResumeState::load(&session_dir);
        assert_eq!(persisted.pending_interactions.len(), 1);
        assert_eq!(persisted.pending_interactions[0].request_id(), "await-me");

        // ...and restore re-attaches it to the new handle before clearing the
        // record, so the interaction is not lost across the gap.
        supervisor.ensure_active(&app.id).await.unwrap();
        assert!(!ResumeState::path_in(&session_dir).is_file());
        let restored = supervisor.pending_interactions(&app.id).await.unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].request_id(), "await-me");

        // The restored lifecycle keeps the pending-interaction block so the
        // sweeper will not immediately re-evict an App still awaiting an answer.
        let blocked = {
            let lifecycles = supervisor.lifecycles.lock().await;
            let state = lifecycles.get(&app.id).unwrap().clone();
            !state.is_idle_evictable(
                std::time::Instant::now() + Duration::from_secs(10_000),
                Duration::from_secs(300),
            )
        };
        assert!(blocked, "a restored App with a pending interaction is not evictable");
    }
}
