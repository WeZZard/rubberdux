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
    /// mutex so the trait's `&self` methods mutate it without a `&mut self`.
    runtimes: Mutex<HashMap<AppId, WorkerHandle>>,
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

        Ok(Self {
            store,
            rpc_addr,
            runtimes: Mutex::new(HashMap::new()),
            inboxes,
            board_tx,
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
                            // No active subscribers is not an error; keep draining
                            // so the worker's stream does not back up.
                            let _ = self.entry_tx.send(EntryNotification { entry, is_final });
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
        let runtimes = self.runtimes.lock().await;
        let handle = runtimes
            .get(id)
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))?;
        handle
            .send_frame(HostToAgent::UserMessage {
                text,
                telegram_message_id: None,
            })
            .await
    }

    async fn subscribe_entries(
        &self,
        id: &AppId,
    ) -> Result<broadcast::Receiver<EntryNotification>, Error> {
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
        Ok(())
    }

    async fn suspend(&self, id: &AppId) -> Result<(), Error> {
        let removed = self.runtimes.lock().await.remove(id);
        if let Some(handle) = removed {
            handle.shutdown();
            self.announce_status(id, AppStatus::Tombstoned);
        }
        Ok(())
    }

    async fn restore(&self, id: &AppId) -> Result<(), Error> {
        if self.runtimes.lock().await.contains_key(id) {
            return Ok(());
        }
        let app = self.require_app(id).await?;
        let session_dir = self.app_session_dir(id);
        let handle = self.start_worker(&app, session_dir).await;
        self.runtimes.lock().await.insert(id.clone(), handle);
        self.announce_status(id, AppStatus::Active);
        Ok(())
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
}
