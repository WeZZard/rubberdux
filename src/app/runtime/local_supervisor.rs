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
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::app::registry::store::AppStore;
use crate::app::runtime::lifecycle::{AppLifecycle, ResumeState, idle_window};
use crate::app::runtime::worker_handle::WorkerHandle;
use crate::app::supervisor::{AppSupervisor, BoardEvent, CreateAppRequest};
use crate::app::{App, AppId, AppStatus, BoardPosition};
use crate::agent::interaction::{AgentInteraction, InteractionResponse};
use crate::agent::runtime::port::EntryNotification;
use crate::error::Error;
use crate::host::{AcceptedWorker, WorkerStream, accept_worker};
use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::trajectory::TrajectoryEvent;

/// Capacity of the per-App entry and trajectory broadcast channels and the board
/// channel. Mirrors `crate::app::supervisor`'s `CHANNEL_CAPACITY`.
const CHANNEL_CAPACITY: usize = 256;

/// Bound on the outbound (host → worker) frame queue per App. Frames are drained
/// promptly by the supervision task, so a small buffer absorbs bursts without
/// unbounded growth.
const OUTBOUND_CAPACITY: usize = 64;

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
}

impl LocalSupervisor {
    /// Bind the loopback RPC listener and start the accept router, returning a
    /// ready supervisor. The router lives for the process lifetime, routing every
    /// worker `Hello` to the matching App's supervision task.
    pub async fn bind(store: Arc<dyn AppStore>) -> Result<Self, Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let rpc_addr = listener.local_addr()?;
        let (board_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let inboxes: Arc<Mutex<HashMap<AppId, ConnectionInbox>>> =
            Arc::new(Mutex::new(HashMap::new()));

        Self::spawn_accept_router(listener, inboxes.clone());

        let supervisor = Self {
            store,
            rpc_addr,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            lifecycles: Arc::new(Mutex::new(HashMap::new())),
            inboxes,
            board_tx,
        };
        supervisor.spawn_idle_sweeper();
        Ok(supervisor)
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
    async fn start_worker(&self, app: &App, app_session_dir: PathBuf) -> WorkerHandle {
        let (entry_tx, _) = broadcast::channel::<EntryNotification>(CHANNEL_CAPACITY);
        let (trajectory_tx, _) = broadcast::channel::<TrajectoryEvent>(CHANNEL_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel::<HostToAgent>(OUTBOUND_CAPACITY);
        let (conn_tx, conn_rx) = mpsc::channel::<WorkerStream>(1);
        let cancel = CancellationToken::new();

        self.inboxes
            .lock()
            .await
            .insert(app.id.clone(), conn_tx);

        let task = SupervisionTask {
            app_id: app.id.clone(),
            rpc_addr: self.rpc_addr,
            app_session_dir,
            entry_tx: entry_tx.clone(),
            cancel: cancel.clone(),
            inboxes: self.inboxes.clone(),
            runtimes: self.runtimes.clone(),
            lifecycles: self.lifecycles.clone(),
        };
        tokio::spawn(task.run(conn_rx, outbound_rx));

        WorkerHandle::new(outbound_tx, entry_tx, trajectory_tx, cancel)
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
        Ok(())
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
) -> Result<(), Error> {
    let removed = runtimes.lock().await.remove(id);
    let Some(handle) = removed else {
        return Ok(());
    };

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
    /// Shared with the supervisor so the RPC pump can record a worker-raised
    /// interaction on this App's [`WorkerHandle`] (a `Tombstoned` suspend later
    /// captures it in `resume.json`). See `docs/app/runtime/worker-lifecycle.md`.
    runtimes: Arc<Mutex<HashMap<AppId, WorkerHandle>>>,
    /// Shared with the supervisor so the RPC pump can update this App's lifecycle
    /// activity flags from real worker events: a final entry finishes the turn,
    /// and an interaction frame sets the pending-interaction block. The idle
    /// sweeper reads these to decide eviction.
    lifecycles: Arc<Mutex<HashMap<AppId, AppLifecycle>>>,
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
    ) {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            let started = std::time::Instant::now();
            let outcome = self
                .spawn_and_pump(&mut conn_rx, &mut outbound_rx)
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
        // dead task.
        self.inboxes.lock().await.remove(&self.app_id);
        log::info!("[app-worker:{}] supervision stopped", self.app_id);
    }

    /// Spawn one child process and pump its RPC duplex until it disconnects, the
    /// task is cancelled, or an error occurs. The child is killed when this
    /// returns so a leaked process never outlives its supervision iteration.
    async fn spawn_and_pump(
        &self,
        conn_rx: &mut mpsc::Receiver<WorkerStream>,
        outbound_rx: &mut mpsc::Receiver<HostToAgent>,
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
