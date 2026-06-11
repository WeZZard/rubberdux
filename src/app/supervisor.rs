//! The App supervisor: the seam that owns every App's agent worker and the
//! observable streams the board renders from. See `docs/app/supervisor.md`.
//!
//! [`AppSupervisor`] is the contract the gateway talks to; it knows nothing
//! about *where* a worker runs. [`MemorySupervisor`] is the in-process
//! realization: it runs one [`AgentLoop`](crate::agent::runtime::agent_loop::AgentLoop)
//! per App as a tokio task and exposes per-App broadcast channels for entries
//! and trajectory events plus a board channel for App lifecycle events. The
//! same trait is later implemented by a subprocess `LocalSupervisor` without
//! the gateway noticing — that out-of-process variant is out of scope here.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::agent::builder::AgentLoopBuilder;
use crate::agent::entry::EntryOrigin;
use crate::agent::interaction::{AgentInteraction, InteractionResponse};
use crate::agent::runtime::port::{EntryNotification, InputPort};
use crate::app::registry::store::AppStore;
use crate::app::{App, AppId, AppStatus, BoardPosition, IconSpec};
use crate::error::Error;
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::session::SessionManager;
use crate::trajectory::{BroadcastTrajectoryRecorder, TrajectoryEvent, noop_recorder};

/// Capacity of the per-App entry and trajectory broadcast channels and the
/// board channel. Subscribers that fall this far behind observe a `Lagged`
/// error and resync; the value mirrors the loop's own entry-notify capacity.
const CHANNEL_CAPACITY: usize = 256;

/// A request to create a new App, carrying just the user-facing identity and
/// board placement the supervisor needs. The originating task prompt is
/// supplied separately to [`AppSupervisor::create_app`] so identity and the
/// first message stay distinct concerns.
#[derive(Debug, Clone)]
pub struct CreateAppRequest {
    pub title: String,
    pub icon: IconSpec,
    pub position: BoardPosition,
}

/// A board-level lifecycle event: an App appeared, changed status, moved, or
/// left the board. The board view renders from this stream so it never has to
/// poll the store. See `docs/app/supervisor.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardEvent {
    /// A new App was created and (for `MemorySupervisor`) its worker started.
    Created(App),
    /// An existing App's worker status changed (e.g. restored to `Active`,
    /// suspended to `Tombstoned`).
    StatusChanged { id: AppId, status: AppStatus },
    /// An App's board position changed.
    Moved { id: AppId, position: BoardPosition },
    /// An App's derived identity (title + icon + summary) replaced the
    /// placeholder it was created with. Carries the updated App so board
    /// subscribers refresh; the board projects it to the same `updated` reload
    /// signal as a status or position change. See `docs/gateway/apps_stream.md`.
    IdentityChanged(App),
    /// An App was archived and removed from the default board listing.
    Archived(AppId),
}

/// The contract every supervisor implements: own each App's worker lifecycle
/// and expose the observable streams the board renders from. Implementations
/// differ only in *where* the worker runs (in-process tokio task here; a
/// subprocess later) — the gateway is written against this trait alone.
///
/// Methods take `&self`: a supervisor is shared behind an `Arc` and serializes
/// its own mutable state internally, so callers never need a `&mut` handle.
pub trait AppSupervisor: Send + Sync {
    /// Create, persist, and (for an in-process supervisor) start a new App.
    /// `initial_prompt` is the originating task message delivered to the new
    /// worker as the first user turn.
    fn create_app(
        &self,
        request: CreateAppRequest,
        initial_prompt: String,
    ) -> impl std::future::Future<Output = Result<App, Error>> + Send;

    /// List Apps on the board. Archived Apps are omitted.
    fn list(&self) -> impl std::future::Future<Output = Result<Vec<App>, Error>> + Send;

    /// Fetch a single App by id, or `None` if it does not exist.
    fn get(
        &self,
        id: &AppId,
    ) -> impl std::future::Future<Output = Result<Option<App>, Error>> + Send;

    /// Deliver a user message to a running App's worker. Errors if the App has
    /// no active worker (it must be restored first).
    fn send_message(
        &self,
        id: &AppId,
        text: String,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send;

    /// Subscribe to an App's history-entry stream. Errors if the App has no
    /// active worker.
    fn subscribe_entries(
        &self,
        id: &AppId,
    ) -> impl std::future::Future<Output = Result<broadcast::Receiver<EntryNotification>, Error>> + Send;

    /// Subscribe to an App's trajectory-event stream. Errors if the App has no
    /// active worker.
    fn subscribe_trajectory(
        &self,
        id: &AppId,
    ) -> impl std::future::Future<Output = Result<broadcast::Receiver<TrajectoryEvent>, Error>> + Send;

    /// The interactions an App's worker has raised and is awaiting a response
    /// for. Errors if the App has no active worker.
    fn pending_interactions(
        &self,
        id: &AppId,
    ) -> impl std::future::Future<Output = Result<Vec<AgentInteraction>, Error>> + Send;

    /// Answer a pending interaction raised by an App's worker. Errors if the
    /// App has no active worker.
    fn respond_to_interaction(
        &self,
        id: &AppId,
        response: InteractionResponse,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send;

    /// Suspend a running App: stop its worker and mark it `Tombstoned`. A no-op
    /// if the App is already suspended.
    fn suspend(&self, id: &AppId) -> impl std::future::Future<Output = Result<(), Error>> + Send;

    /// Restore a suspended App: start its worker again and mark it `Active`. A
    /// no-op if the App is already active.
    fn restore(&self, id: &AppId) -> impl std::future::Future<Output = Result<(), Error>> + Send;

    /// Archive an App: suspend any worker and move it off the default board.
    fn archive(&self, id: &AppId) -> impl std::future::Future<Output = Result<(), Error>> + Send;

    /// Move an App to a new board position.
    fn move_app(
        &self,
        id: &AppId,
        position: BoardPosition,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send;

    /// Subscribe to the board's App-lifecycle event stream.
    fn subscribe_board(&self) -> broadcast::Receiver<BoardEvent>;
}

/// Handle to one running App worker: the channels and tokens the supervisor
/// needs to message it, observe it, and shut it down. Held only while the App
/// is `Active`; dropped on suspend/archive so the worker task exits.
struct AppRuntime {
    /// Sends user messages and interaction responses into the worker's loop.
    input_port: InputPort,
    /// Re-broadcasts the loop's entry notifications to board subscribers.
    entry_tx: broadcast::Sender<EntryNotification>,
    /// Broadcasts the worker's trajectory events to board subscribers.
    trajectory_tx: broadcast::Sender<TrajectoryEvent>,
    /// Interactions the worker raised and is awaiting an answer for.
    pending_interactions: Vec<AgentInteraction>,
    /// Cancels the worker's tokio task on suspend/archive.
    cancel: CancellationToken,
}

/// In-process [`AppSupervisor`]: runs one [`AgentLoop`](crate::agent::runtime::agent_loop::AgentLoop)
/// per App as a tokio task, reusing the spawn pattern from
/// `crate::agent::runtime::subagent`. Persistence is delegated to the injected
/// [`AppStore`]; live runtimes are tracked in memory keyed by [`AppId`].
pub struct MemorySupervisor {
    client: Arc<MoonshotClient>,
    session_manager: Arc<SessionManager>,
    store: Arc<dyn AppStore>,
    /// Live worker handles for `Active` Apps. Guarded by an async mutex so the
    /// trait's `&self` methods can mutate it without a `&mut self` handle.
    runtimes: Mutex<HashMap<AppId, AppRuntime>>,
    /// Board lifecycle event fan-out, kept alive for the supervisor's lifetime
    /// so late subscribers can attach.
    board_tx: broadcast::Sender<BoardEvent>,
}

impl MemorySupervisor {
    /// Construct a supervisor over the given client, session manager, and store.
    pub fn new(
        client: Arc<MoonshotClient>,
        session_manager: Arc<SessionManager>,
        store: Arc<dyn AppStore>,
    ) -> Self {
        let (board_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            client,
            session_manager,
            store,
            runtimes: Mutex::new(HashMap::new()),
            board_tx,
        }
    }

    /// Build and spawn a worker for `app`, returning its runtime handle. Mirrors
    /// the `subagent::spawn_subagent` pattern: subscribe the loop's output
    /// before driving it, then re-broadcast entries to the App's own channel.
    async fn spawn_worker(&self, app: &App) -> Result<AppRuntime, Error> {
        let (session_id, _) = self
            .session_manager
            .create_session(self.client.model().to_owned())
            .map_err(|e| Error::App(format!("failed to create session for App `{}`: {e}", app.id)))?;

        let (entry_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (trajectory_tx, _) = broadcast::channel(CHANNEL_CAPACITY);

        // Wrap a no-op inner recorder so the loop's trajectory events fan out to
        // the App's trajectory channel without also persisting twice.
        let recorder: crate::trajectory::SharedTrajectoryRecorder = Arc::new(
            BroadcastTrajectoryRecorder::new(noop_recorder(), trajectory_tx.clone()),
        );

        let builder = AgentLoopBuilder::new(app.summary.clone(), self.session_manager.clone())
            .with_session_id(session_id)
            .with_recorder(recorder);

        let (agent_loop, input_port, _context_tx) = builder.build(self.client.clone()).await;

        // Subscribe before `run()` so no entry is missed between spawn and the
        // first observation; forward every notification to the App's channel.
        let mut output = agent_loop.subscribe_output();
        let entry_tx_forward = entry_tx.clone();
        tokio::spawn(async move {
            while let Some(notification) = output.recv().await {
                if entry_tx_forward.send(notification).is_err() {
                    // No active subscribers right now; keep draining so the
                    // loop's broadcast buffer does not back up.
                    continue;
                }
            }
        });

        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = run_cancel.cancelled() => {}
                _ = agent_loop.run() => {}
            }
        });

        Ok(AppRuntime {
            input_port,
            entry_tx,
            trajectory_tx,
            pending_interactions: Vec::new(),
            cancel,
        })
    }

    /// Persist an App's status to the store's manifest by re-creating the
    /// in-memory snapshot, then announce the change on the board channel. The
    /// store always loads Apps as `Tombstoned`, so `Active` is a runtime-only
    /// fact the board learns from this event rather than from disk.
    fn announce_status(&self, id: &AppId, status: AppStatus) {
        let _ = self.board_tx.send(BoardEvent::StatusChanged {
            id: id.clone(),
            status,
        });
    }

    /// Look up the live App from the store, erroring if it does not exist.
    async fn require_app(&self, id: &AppId) -> Result<App, Error> {
        self.store
            .get(id)?
            .ok_or_else(|| Error::App(format!("App `{id}` does not exist")))
    }
}

impl AppSupervisor for MemorySupervisor {
    async fn create_app(
        &self,
        request: CreateAppRequest,
        initial_prompt: String,
    ) -> Result<App, Error> {
        let mut app = App::new(
            AppId::now(),
            request.title,
            request.icon,
            request.position,
        );
        // The summary doubles as the worker's system prompt seed at this seam;
        // identity/title generation is designed and implemented elsewhere.
        app.summary = initial_prompt.clone();
        self.store.create(&app)?;

        let runtime = self.spawn_worker(&app).await?;

        // Deliver the originating task as the worker's first user turn.
        let message = Message::User {
            content: UserContent::Text(initial_prompt),
        };
        runtime
            .input_port
            .send_user_message(message, EntryOrigin::System)
            .await?;

        let mut active = app.clone();
        active.status = AppStatus::Active;
        self.runtimes.lock().await.insert(app.id.clone(), runtime);

        let _ = self.board_tx.send(BoardEvent::Created(active.clone()));
        Ok(active)
    }

    async fn list(&self) -> Result<Vec<App>, Error> {
        let mut apps = self.store.list(false)?;
        // Reflect runtime status: the store loads everything Tombstoned, but
        // Apps with a live worker are Active.
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
        let runtime = runtimes
            .get(id)
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))?;
        let message = Message::User {
            content: UserContent::Text(text),
        };
        runtime
            .input_port
            .send_user_message(message, EntryOrigin::User { channel: "board".into() })
            .await
    }

    async fn subscribe_entries(
        &self,
        id: &AppId,
    ) -> Result<broadcast::Receiver<EntryNotification>, Error> {
        let runtimes = self.runtimes.lock().await;
        runtimes
            .get(id)
            .map(|runtime| runtime.entry_tx.subscribe())
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))
    }

    async fn subscribe_trajectory(
        &self,
        id: &AppId,
    ) -> Result<broadcast::Receiver<TrajectoryEvent>, Error> {
        let runtimes = self.runtimes.lock().await;
        runtimes
            .get(id)
            .map(|runtime| runtime.trajectory_tx.subscribe())
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))
    }

    async fn pending_interactions(&self, id: &AppId) -> Result<Vec<AgentInteraction>, Error> {
        let runtimes = self.runtimes.lock().await;
        runtimes
            .get(id)
            .map(|runtime| runtime.pending_interactions.clone())
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))
    }

    async fn respond_to_interaction(
        &self,
        id: &AppId,
        response: InteractionResponse,
    ) -> Result<(), Error> {
        let mut runtimes = self.runtimes.lock().await;
        let runtime = runtimes
            .get_mut(id)
            .ok_or_else(|| Error::App(format!("App `{id}` has no active worker")))?;
        // Drop the matching pending interaction; routing the response back into
        // the worker's loop is wired when interaction handling lands. See
        // docs/agent/interaction.md.
        runtime
            .pending_interactions
            .retain(|interaction| interaction.request_id() != response.request_id());
        Ok(())
    }

    async fn suspend(&self, id: &AppId) -> Result<(), Error> {
        let removed = self.runtimes.lock().await.remove(id);
        if let Some(runtime) = removed {
            runtime.cancel.cancel();
            self.announce_status(id, AppStatus::Tombstoned);
        }
        Ok(())
    }

    async fn restore(&self, id: &AppId) -> Result<(), Error> {
        if self.runtimes.lock().await.contains_key(id) {
            return Ok(());
        }
        let app = self.require_app(id).await?;
        let runtime = self.spawn_worker(&app).await?;
        self.runtimes.lock().await.insert(id.clone(), runtime);
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
        // Persisting the new position is owned by the store's manifest writer,
        // which gains a position-update path in a later task; here the seam
        // announces the move so the board stays live.
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

    /// A client that never reaches the network in these tests: messages produce
    /// a user entry on the loop's broadcast before any LLM turn is driven, which
    /// is all the entry-subscription test observes.
    fn dummy_client() -> Arc<MoonshotClient> {
        Arc::new(MoonshotClient::new(
            reqwest::Client::new(),
            "http://localhost:0".into(),
            "test-key".into(),
            "test-model".into(),
        ))
    }

    fn temp_supervisor() -> MemorySupervisor {
        let home = tempfile::tempdir().unwrap().into_path();
        let session_manager = Arc::new(SessionManager {
            home_dir: home.clone(),
            sessions_dir: home.join("sessions"),
            latest_link: home.join("latest"),
        });
        let store: Arc<dyn AppStore> =
            Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));
        MemorySupervisor::new(dummy_client(), session_manager, store)
    }

    fn sample_request() -> CreateAppRequest {
        CreateAppRequest {
            title: "Plan the trip".into(),
            icon: IconSpec {
                symbol: "airplane".into(),
                color: "#3478F6".into(),
            },
            position: BoardPosition { row: 1, column: 1 },
        }
    }

    #[tokio::test]
    async fn create_app_then_get_and_list() {
        let supervisor = temp_supervisor();

        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        // A freshly created App has a live worker, so it reads as Active.
        assert_eq!(app.status, AppStatus::Active);

        let fetched = supervisor.get(&app.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, app.id);
        assert_eq!(fetched.status, AppStatus::Active);

        let listed = supervisor.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, app.id);
        assert_eq!(listed[0].status, AppStatus::Active);
    }

    #[tokio::test]
    async fn create_app_emits_board_created_event() {
        let supervisor = temp_supervisor();
        let mut board = supervisor.subscribe_board();

        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        match board.recv().await.unwrap() {
            BoardEvent::Created(created) => assert_eq!(created.id, app.id),
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_message_produces_entry_on_subscribe_entries() {
        let supervisor = temp_supervisor();
        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        let mut entries = supervisor.subscribe_entries(&app.id).await.unwrap();
        supervisor
            .send_message(&app.id, "hello there".into())
            .await
            .unwrap();

        // The loop broadcasts the user entry before driving any LLM turn, so a
        // matching entry must arrive on the subscription.
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(notification) = entries.recv().await {
                    if notification.entry.message.content_text() == "hello there" {
                        return true;
                    }
                } else {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(found, "expected the sent message to surface as an entry");
    }

    #[tokio::test]
    async fn subscribe_entries_errors_without_worker() {
        let supervisor = temp_supervisor();
        let missing = AppId("2026-06-10-00-00-00-UTC".into());
        assert!(matches!(
            supervisor.subscribe_entries(&missing).await,
            Err(Error::App(_))
        ));
    }

    #[tokio::test]
    async fn suspend_then_restore_round_trips_status() {
        let supervisor = temp_supervisor();
        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        supervisor.suspend(&app.id).await.unwrap();
        assert_eq!(
            supervisor.get(&app.id).await.unwrap().unwrap().status,
            AppStatus::Tombstoned
        );
        // No active worker after suspend.
        assert!(matches!(
            supervisor.send_message(&app.id, "x".into()).await,
            Err(Error::App(_))
        ));

        supervisor.restore(&app.id).await.unwrap();
        assert_eq!(
            supervisor.get(&app.id).await.unwrap().unwrap().status,
            AppStatus::Active
        );
        // Messaging works again once restored.
        supervisor.send_message(&app.id, "y".into()).await.unwrap();
    }

    #[tokio::test]
    async fn archive_suspends_and_removes_from_list() {
        let supervisor = temp_supervisor();
        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        supervisor.archive(&app.id).await.unwrap();

        assert!(supervisor.list().await.unwrap().is_empty());
        // Still addressable by id (archived, Tombstoned).
        let fetched = supervisor.get(&app.id).await.unwrap().unwrap();
        assert_eq!(fetched.status, AppStatus::Tombstoned);
    }

    #[tokio::test]
    async fn move_app_emits_board_moved_event() {
        let supervisor = temp_supervisor();
        let mut board = supervisor.subscribe_board();
        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();
        // Drain the Created event.
        let _ = board.recv().await.unwrap();

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
}
