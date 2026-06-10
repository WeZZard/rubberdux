//! The multi-App board REST surface and its DTOs. See `docs/gateway/apps.md`.
//!
//! These routes expose the whiteboard's Apps over HTTP, backed by the
//! [`AppSupervisor`](crate::app::supervisor::AppSupervisor) seam. The handlers
//! talk to the supervisor through the object-safe [`DynAppSupervisor`] adapter
//! (the upstream trait uses bare `async fn`, which is not `dyn`-compatible; the
//! adapter boxes each future so the gateway can hold one supervisor behind an
//! `Arc<dyn …>` regardless of where its workers run). The single-agent entry
//! endpoints in `super::route` are unaffected; these routes are additive.
//!
//! Two responsiveness rules from the root conventions shape this surface:
//! creating an App returns immediately and derives its identity (title + icon)
//! on a background task, and sending a task returns `202 Accepted` — neither
//! handler blocks on agent work.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agent::interaction::{AgentInteraction, InteractionResponse};
use crate::agent::runtime::port::EntryNotification;
use crate::app::identity::derive_identity;
use crate::app::merge::clustering::LlmClusterer;
use crate::app::merge::{ClusterCandidate, ClusterDecision, Clusterer};
use crate::app::registry::store::{
    AppStore, FilesystemAppStore, MemberRecord, MergeDecisionKind, MergeRecord,
};
use crate::app::supervisor::{AppSupervisor, CreateAppRequest};
use crate::app::{App, AppId, BoardPosition, IconSpec};
use crate::error::Error;
use crate::trajectory::TrajectoryEvent;

use super::error::GatewayError;
use super::state::GatewayState;

// ---------------------------------------------------------------------------
// Object-safe supervisor adapter
// ---------------------------------------------------------------------------

/// Object-safe view of [`AppSupervisor`]. The source trait uses bare
/// `async fn`, whose return types are not nameable in a vtable, so the trait
/// itself is not `dyn`-compatible. This adapter restates each method returning a
/// boxed future, and a blanket impl forwards to any `AppSupervisor`, so the
/// gateway can store `Arc<dyn DynAppSupervisor>` without depending on a concrete
/// supervisor or on where its workers run. See `docs/gateway/apps.md`.
pub trait DynAppSupervisor: Send + Sync {
    fn create_app(
        &self,
        request: CreateAppRequest,
        initial_prompt: String,
    ) -> BoxFuture<'_, Result<App, Error>>;

    fn list(&self) -> BoxFuture<'_, Result<Vec<App>, Error>>;

    fn get<'a>(&'a self, id: &'a AppId) -> BoxFuture<'a, Result<Option<App>, Error>>;

    fn send_message<'a>(
        &'a self,
        id: &'a AppId,
        text: String,
    ) -> BoxFuture<'a, Result<(), Error>>;

    fn subscribe_entries<'a>(
        &'a self,
        id: &'a AppId,
    ) -> BoxFuture<'a, Result<broadcast::Receiver<EntryNotification>, Error>>;

    fn subscribe_trajectory<'a>(
        &'a self,
        id: &'a AppId,
    ) -> BoxFuture<'a, Result<broadcast::Receiver<TrajectoryEvent>, Error>>;

    fn pending_interactions<'a>(
        &'a self,
        id: &'a AppId,
    ) -> BoxFuture<'a, Result<Vec<AgentInteraction>, Error>>;

    fn respond_to_interaction<'a>(
        &'a self,
        id: &'a AppId,
        response: InteractionResponse,
    ) -> BoxFuture<'a, Result<(), Error>>;

    fn restore<'a>(&'a self, id: &'a AppId) -> BoxFuture<'a, Result<(), Error>>;

    fn archive<'a>(&'a self, id: &'a AppId) -> BoxFuture<'a, Result<(), Error>>;

    fn move_app<'a>(
        &'a self,
        id: &'a AppId,
        position: BoardPosition,
    ) -> BoxFuture<'a, Result<(), Error>>;
}

impl<T: AppSupervisor> DynAppSupervisor for T {
    fn create_app(
        &self,
        request: CreateAppRequest,
        initial_prompt: String,
    ) -> BoxFuture<'_, Result<App, Error>> {
        Box::pin(AppSupervisor::create_app(self, request, initial_prompt))
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<App>, Error>> {
        Box::pin(AppSupervisor::list(self))
    }

    fn get<'a>(&'a self, id: &'a AppId) -> BoxFuture<'a, Result<Option<App>, Error>> {
        Box::pin(AppSupervisor::get(self, id))
    }

    fn send_message<'a>(
        &'a self,
        id: &'a AppId,
        text: String,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(AppSupervisor::send_message(self, id, text))
    }

    fn subscribe_entries<'a>(
        &'a self,
        id: &'a AppId,
    ) -> BoxFuture<'a, Result<broadcast::Receiver<EntryNotification>, Error>> {
        Box::pin(AppSupervisor::subscribe_entries(self, id))
    }

    fn subscribe_trajectory<'a>(
        &'a self,
        id: &'a AppId,
    ) -> BoxFuture<'a, Result<broadcast::Receiver<TrajectoryEvent>, Error>> {
        Box::pin(AppSupervisor::subscribe_trajectory(self, id))
    }

    fn pending_interactions<'a>(
        &'a self,
        id: &'a AppId,
    ) -> BoxFuture<'a, Result<Vec<AgentInteraction>, Error>> {
        Box::pin(AppSupervisor::pending_interactions(self, id))
    }

    fn respond_to_interaction<'a>(
        &'a self,
        id: &'a AppId,
        response: InteractionResponse,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(AppSupervisor::respond_to_interaction(self, id, response))
    }

    fn restore<'a>(&'a self, id: &'a AppId) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(AppSupervisor::restore(self, id))
    }

    fn archive<'a>(&'a self, id: &'a AppId) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(AppSupervisor::archive(self, id))
    }

    fn move_app<'a>(
        &'a self,
        id: &'a AppId,
        position: BoardPosition,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(AppSupervisor::move_app(self, id, position))
    }
}

/// Map a supervisor-layer [`Error`] into the gateway's HTTP error vocabulary.
/// The seam reports a missing App as `Error::App`, which the board surface
/// renders as a generic supervisor failure unless a handler has already proven
/// the App absent (in which case it returns [`GatewayError::AppNotFound`]).
fn supervisor_error(error: Error) -> GatewayError {
    GatewayError::Supervisor(error.to_string())
}

/// The supervisor wired into the gateway, or a `503`-shaped error when the
/// gateway was built without the board surface (the single-agent constructors
/// leave it unset). See `docs/gateway/apps.md`.
fn require_supervisor(
    state: &GatewayState,
) -> Result<&Arc<dyn DynAppSupervisor>, GatewayError> {
    state
        .supervisor
        .as_ref()
        .ok_or_else(|| GatewayError::Supervisor("app board is not enabled".into()))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Board position as it crosses the REST boundary. Mirrors
/// [`BoardPosition`] but is named for the API so the wire shape is stable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BoardPositionDto {
    pub row: i32,
    pub column: i32,
}

impl From<BoardPosition> for BoardPositionDto {
    fn from(value: BoardPosition) -> Self {
        Self {
            row: value.row,
            column: value.column,
        }
    }
}

impl From<BoardPositionDto> for BoardPosition {
    fn from(value: BoardPositionDto) -> Self {
        Self {
            row: value.row,
            column: value.column,
        }
    }
}

/// An App icon as it crosses the REST boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IconDto {
    pub symbol: String,
    pub color: String,
}

impl From<IconSpec> for IconDto {
    fn from(value: IconSpec) -> Self {
        Self {
            symbol: value.symbol,
            color: value.color,
        }
    }
}

/// The board view of an [`App`]: identity, placement, status, and the topic the
/// user sees. The internal `member_session_ids` membership log is omitted from
/// the wire shape, which is a board concern, not a client one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppDto {
    pub id: String,
    pub title: String,
    pub icon: IconDto,
    pub position: BoardPositionDto,
    pub status: String,
    pub summary: String,
    pub user_locked: bool,
    pub last_active: String,
}

impl From<App> for AppDto {
    fn from(app: App) -> Self {
        let status = match app.status {
            crate::app::AppStatus::Active => "active",
            crate::app::AppStatus::Tombstoned => "tombstoned",
        };
        Self {
            id: app.id.0,
            title: app.title,
            icon: app.icon.into(),
            position: app.position.into(),
            status: status.to_owned(),
            summary: app.summary,
            user_locked: app.user_locked,
            last_active: app.last_active,
        }
    }
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/// Body for `POST /apps`. The originating `task` seeds the worker's first user
/// turn and drives background identity derivation; placement is required so the
/// new tile lands somewhere deterministic on the board.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateAppBody {
    pub task: String,
    pub position: BoardPositionDto,
}

/// Body for `PATCH /apps/{id}`. Every field is optional; only the supplied ones
/// are applied. Today the supervisor seam can apply a position change; other
/// fields are accepted for forward compatibility and ignored until their
/// persistence path lands.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchAppBody {
    pub position: Option<BoardPositionDto>,
    pub title: Option<String>,
    pub user_locked: Option<bool>,
}

/// Body for `POST /apps/{id}/tasks`: a free-form user message delivered to the
/// App's running worker.
#[derive(Debug, Clone, Deserialize)]
pub struct SendTaskBody {
    pub text: String,
}

// ---------------------------------------------------------------------------
// Response bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AppsListResponse {
    apps: Vec<AppDto>,
}

#[derive(Debug, Serialize)]
struct EntriesResponse {
    entries: Vec<crate::agent::entry::Entry>,
}

#[derive(Debug, Serialize)]
struct TrajectoryResponse {
    events: Vec<TrajectoryEvent>,
}

#[derive(Debug, Serialize)]
struct InteractionsResponse {
    interactions: Vec<AgentInteraction>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The board REST routes, to be merged into the gateway router by
/// `super::route`/`super::server`. Additive to the single-agent surface.
pub fn router() -> axum::Router<Arc<GatewayState>> {
    axum::Router::new()
        .route("/api/v1/apps", get(list_apps).post(create_app))
        .route(
            "/api/v1/apps/{id}",
            get(get_app).patch(patch_app).delete(archive_app),
        )
        .route("/api/v1/apps/{id}/restore", post(restore_app))
        .route("/api/v1/apps/{id}/tasks", post(send_task))
        .route("/api/v1/apps/{id}/entries", get(list_entries))
        .route("/api/v1/apps/{id}/trajectory", get(list_trajectory))
        .route("/api/v1/apps/{id}/interactions", get(list_interactions))
        .route(
            "/api/v1/apps/{id}/interactions/{request_id}",
            post(respond_interaction),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_apps(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<AppsListResponse>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let apps = supervisor.list().await.map_err(supervisor_error)?;
    Ok(Json(AppsListResponse {
        apps: apps.into_iter().map(AppDto::from).collect(),
    }))
}

/// Create an App and return immediately. The supervisor persists and starts the
/// App with a heuristic identity; deriving the title + icon from the task hits
/// the LLM, so it runs on a background task per the root convention that the
/// chat handler must never block. The created App is returned right away.
///
/// Auto-merge clustering also runs off this path on a background task: it
/// consults the [`Clusterer`] against the other (non-`user_locked`) Apps and,
/// when the new conversation clearly belongs with an existing App, routes the
/// originating task into that App's worker, bumps its `last_active`, and records
/// the merge in `merge_log.jsonl`. The decision never blocks the request, and a
/// `Join` is additive — the freshly created tile is still returned. See
/// `docs/app/merge/clustering.md`.
async fn create_app(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<CreateAppBody>,
) -> Result<(StatusCode, Json<AppDto>), GatewayError> {
    let supervisor = require_supervisor(&state)?;

    // Seed the tile with a heuristic identity so the board has something to
    // render instantly; the LLM-derived identity replaces it in the background.
    let request = CreateAppRequest {
        title: body.task.clone(),
        icon: IconSpec {
            symbol: "doc".into(),
            color: "#8E8E93".into(),
        },
        position: body.position.into(),
    };

    let app = supervisor
        .create_app(request, body.task.clone())
        .await
        .map_err(supervisor_error)?;

    // Derive the real identity off the request path. The board observes the
    // result via the App's streams once the identity-update path lands; failing
    // to derive is non-fatal because `derive_identity` itself never errors.
    if let Some(client) = state.identity_client.clone() {
        let task = body.task.clone();
        let app_id = app.id.clone();
        tokio::spawn(async move {
            let identity = derive_identity(&client, &task).await;
            log::info!(
                "derived identity for App `{}`: title={:?} symbol={:?}",
                app_id,
                identity.title,
                identity.icon.symbol
            );
        });
    }

    // Consult the clusterer off the request path. Reuses the gateway's existing
    // identity client (a `MoonshotClient`) — no new state field — and the
    // supervisor already in state. See `docs/app/merge/clustering.md`.
    if let (Some(client), Some(supervisor)) =
        (state.identity_client.clone(), state.supervisor.clone())
    {
        let new_app_id = app.id.clone();
        let summary = body.task.clone();
        tokio::spawn(async move {
            consult_clusterer(supervisor, client, new_app_id, summary).await;
        });
    }

    Ok((StatusCode::CREATED, Json(app.into())))
}

/// Run the auto-merge decision for a just-created App and apply it. Lists the
/// other non-`user_locked` Apps as candidates, classifies via the
/// [`LlmClusterer`], and on a `Join` routes the originating task into the
/// matched App's worker (which makes the merged App the most-recently-active,
/// the live `last_active` bump), records the merge in `merge_log.jsonl`, and
/// records the merged session as a new member. Every branch is best-effort and
/// non-fatal: this runs on a background task, so failures are logged, not
/// propagated. See `docs/app/merge/clustering.md`.
async fn consult_clusterer(
    supervisor: Arc<dyn DynAppSupervisor>,
    client: Arc<crate::provider::moonshot::MoonshotClient>,
    new_app_id: AppId,
    summary: String,
) {
    let apps = match supervisor.list().await {
        Ok(apps) => apps,
        Err(e) => {
            log::warn!("clustering: failed to list candidate Apps: {e}");
            return;
        }
    };

    // Candidates exclude the just-created App and every `user_locked` App: the
    // clusterer must never override a user's pin.
    let candidates: Vec<ClusterCandidate> = apps
        .into_iter()
        .filter(|a| a.id != new_app_id && !a.user_locked)
        .map(|a| ClusterCandidate {
            app_id: a.id.0,
            summary: a.summary,
        })
        .collect();

    let clusterer = LlmClusterer::new(client);
    let decision = clusterer.classify(&summary, &candidates).await;

    // Every decision is recorded so the cluster history is auditable. The store
    // resolves `RUBBERDUX_HOME` the same way the supervisor's does, so it sees
    // the same on-disk Apps.
    let store = FilesystemAppStore::new();
    let now = chrono::Utc::now().to_rfc3339();
    match decision {
        ClusterDecision::Join { app_id } => {
            let target = AppId(app_id);
            log::info!("clustering: merging App `{new_app_id}` into `{target}`");

            // Route the originating conversation into the existing App's worker.
            if let Err(e) = supervisor.send_message(&target, summary.clone()).await {
                log::warn!("clustering: failed to route merged message into `{target}`: {e}");
            }

            // Bump and persist the target App's `last_active` so the merge makes
            // it genuinely most-recently-used in board ordering, independent of
            // any worker-side effect of routing the message.
            if let Err(e) = store.touch_last_active(&target) {
                log::warn!("clustering: failed to bump last_active for `{target}`: {e}");
            }

            // Record the merge in the target's append-only logs so the cluster's
            // history is auditable.
            let merge = MergeRecord {
                kind: MergeDecisionKind::Join,
                source_app_id: new_app_id.0.clone(),
                recorded_at: now.clone(),
            };
            if let Err(e) = store.record_merge(&target, &merge) {
                log::warn!("clustering: failed to append merge_log for `{target}`: {e}");
            }
            let member = MemberRecord {
                session_id: new_app_id.0.clone(),
                recorded_at: now,
                joined: true,
            };
            if let Err(e) = store.record_member(&target, &member) {
                log::warn!("clustering: failed to append members log for `{target}`: {e}");
            }
        }
        ClusterDecision::New => {
            log::info!("clustering: App `{new_app_id}` stays a new app");

            // A `New` decision is logged against the new App itself, so every
            // clusterer decision — not only merges — is recorded.
            let record = MergeRecord {
                kind: MergeDecisionKind::New,
                source_app_id: new_app_id.0.clone(),
                recorded_at: now,
            };
            if let Err(e) = store.record_merge(&new_app_id, &record) {
                log::warn!("clustering: failed to append merge_log for `{new_app_id}`: {e}");
            }
        }
    }
}

async fn get_app(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Result<Json<AppDto>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    let app = supervisor
        .get(&app_id)
        .await
        .map_err(supervisor_error)?
        .ok_or(GatewayError::AppNotFound(id))?;
    Ok(Json(app.into()))
}

/// Apply a partial update. The supervisor seam can move an App; a position in
/// the body is forwarded to it. Other fields are accepted for forward
/// compatibility and currently ignored. Returns the App's current view.
async fn patch_app(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Json(body): Json<PatchAppBody>,
) -> Result<Json<AppDto>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());

    // Prove the App exists before mutating, so an unknown id is a 404 rather
    // than a generic supervisor error.
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }

    if let Some(position) = body.position {
        supervisor
            .move_app(&app_id, position.into())
            .await
            .map_err(supervisor_error)?;
    }

    let app = supervisor
        .get(&app_id)
        .await
        .map_err(supervisor_error)?
        .ok_or(GatewayError::AppNotFound(id))?;
    Ok(Json(app.into()))
}

async fn archive_app(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    supervisor.archive(&app_id).await.map_err(supervisor_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn restore_app(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Result<Json<AppDto>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    supervisor.restore(&app_id).await.map_err(supervisor_error)?;
    let app = supervisor
        .get(&app_id)
        .await
        .map_err(supervisor_error)?
        .ok_or(GatewayError::AppNotFound(id))?;
    Ok(Json(app.into()))
}

/// Deliver a task message to an App's worker and return `202 Accepted`: the
/// work is handed to the agent loop and proceeds asynchronously, so the handler
/// never blocks on agent progress.
async fn send_task(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    Json(body): Json<SendTaskBody>,
) -> Result<StatusCode, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    supervisor
        .send_message(&app_id, body.text)
        .await
        .map_err(supervisor_error)?;
    Ok(StatusCode::ACCEPTED)
}

/// The App's current history entries, captured as a one-shot snapshot of its
/// entry stream. A live feed is the WebSocket surface's concern (next task).
async fn list_entries(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Result<Json<EntriesResponse>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    let mut receiver = supervisor
        .subscribe_entries(&app_id)
        .await
        .map_err(supervisor_error)?;
    let entries = drain(&mut receiver, |n: EntryNotification| n.entry);
    Ok(Json(EntriesResponse { entries }))
}

/// The App's current trajectory events, captured as a one-shot snapshot of its
/// trajectory stream.
async fn list_trajectory(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Result<Json<TrajectoryResponse>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    let mut receiver = supervisor
        .subscribe_trajectory(&app_id)
        .await
        .map_err(supervisor_error)?;
    let events = drain(&mut receiver, |event: TrajectoryEvent| event);
    Ok(Json(TrajectoryResponse { events }))
}

async fn list_interactions(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Result<Json<InteractionsResponse>, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    let interactions = supervisor
        .pending_interactions(&app_id)
        .await
        .map_err(supervisor_error)?;
    Ok(Json(InteractionsResponse { interactions }))
}

/// Answer a pending interaction. The body is the interaction response; its
/// `request_id` must match the path segment, otherwise the request is rejected
/// as a missing interaction.
async fn respond_interaction(
    State(state): State<Arc<GatewayState>>,
    Path((id, request_id)): Path<(String, String)>,
    Json(response): Json<InteractionResponse>,
) -> Result<StatusCode, GatewayError> {
    let supervisor = require_supervisor(&state)?;
    let app_id = AppId(id.clone());
    if supervisor.get(&app_id).await.map_err(supervisor_error)?.is_none() {
        return Err(GatewayError::AppNotFound(id));
    }
    if response.request_id() != request_id {
        return Err(GatewayError::InteractionNotFound(request_id));
    }
    supervisor
        .respond_to_interaction(&app_id, response)
        .await
        .map_err(supervisor_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drain everything currently buffered on a broadcast receiver into a vector,
/// mapping each item. Used to turn a live stream into a REST snapshot without
/// blocking; a lag is treated as the end of the buffered backlog.
fn drain<T, U>(
    receiver: &mut broadcast::Receiver<T>,
    map: impl Fn(T) -> U,
) -> Vec<U>
where
    T: Clone,
{
    let mut out = Vec::new();
    while let Ok(item) = receiver.try_recv() {
        out.push(map(item));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::agent::runtime::port::{InputPort, LoopEvent};
    use crate::app::registry::store::{AppStore, FilesystemAppStore};
    use crate::app::supervisor::MemorySupervisor;
    use crate::provider::moonshot::MoonshotClient;
    use crate::session::SessionManager;

    /// Serializes tests that mutate the process-global `RUBBERDUX_HOME` so the
    /// clusterer's `FilesystemAppStore::new()` resolves to the same temp home its
    /// supervisor uses, without racing other tests on the env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn dummy_input_port() -> InputPort {
        let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
        InputPort::new(tx)
    }

    fn dummy_client() -> Arc<MoonshotClient> {
        Arc::new(MoonshotClient::new(
            reqwest::Client::new(),
            "http://localhost:0".into(),
            "test-key".into(),
            "test-model".into(),
        ))
    }

    fn memory_supervisor() -> Arc<MemorySupervisor> {
        let home = tempfile::tempdir().unwrap().into_path();
        let session_manager = Arc::new(SessionManager {
            home_dir: home.clone(),
            sessions_dir: home.join("sessions"),
            latest_link: home.join("latest"),
        });
        let store: Arc<dyn AppStore> =
            Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));
        Arc::new(MemorySupervisor::new(dummy_client(), session_manager, store))
    }

    fn app_router() -> axum::Router {
        let state = Arc::new(GatewayState::with_apps(
            "sys".into(),
            "id".into(),
            "soul".into(),
            dummy_input_port(),
            memory_supervisor(),
            dummy_client(),
        ));
        router().with_state(state)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(value) => builder
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&value).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(request).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    #[tokio::test]
    async fn create_then_list_get_patch_archive_lifecycle() {
        let app = app_router();

        // Create returns 201 immediately with the new App.
        let (status, created) = send(
            &app,
            "POST",
            "/api/v1/apps",
            Some(serde_json::json!({
                "task": "Plan the offsite",
                "position": { "row": 1, "column": 2 }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = created["id"].as_str().unwrap().to_owned();
        assert_eq!(created["status"], "active");

        // List shows the created App.
        let (status, list) = send(&app, "GET", "/api/v1/apps", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["apps"].as_array().unwrap().len(), 1);

        // Get returns it by id.
        let (status, got) =
            send(&app, "GET", &format!("/api/v1/apps/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["id"], id);

        // Patch the board position.
        let (status, _patched) = send(
            &app,
            "PATCH",
            &format!("/api/v1/apps/{id}"),
            Some(serde_json::json!({ "position": { "row": 4, "column": 9 } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Archive removes it from the board.
        let (status, _) =
            send(&app, "DELETE", &format!("/api/v1/apps/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, list) = send(&app, "GET", "/api/v1/apps", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(list["apps"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_task_returns_202() {
        let app = app_router();
        let (_, created) = send(
            &app,
            "POST",
            "/api/v1/apps",
            Some(serde_json::json!({
                "task": "Do the thing",
                "position": { "row": 0, "column": 0 }
            })),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_owned();

        let (status, _) = send(
            &app,
            "POST",
            &format!("/api/v1/apps/{id}/tasks"),
            Some(serde_json::json!({ "text": "next step" })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn unknown_app_id_is_404() {
        let app = app_router();
        let (status, _) = send(
            &app,
            "GET",
            "/api/v1/apps/2000-01-01-00-00-00-UTC",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(
            &app,
            "POST",
            "/api/v1/apps/2000-01-01-00-00-00-UTC/tasks",
            Some(serde_json::json!({ "text": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A `Join` decision must genuinely bump and persist the target App's
    /// `last_active`, so MRU board ordering reflects the merge. Drives
    /// `consult_clusterer` directly with the lexical fallback forced to `Join`
    /// (the dummy client points at an unreachable URL, and the candidate's
    /// summary is identical to the new conversation's, so Jaccard is 1.0).
    #[tokio::test]
    async fn join_merge_bumps_and_persists_last_active() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        // The clusterer's `FilesystemAppStore::new()` resolves `RUBBERDUX_HOME`;
        // point it at the same temp home the supervisor's store roots under so
        // both see the same Apps on disk.
        let home = tempfile::tempdir().unwrap().into_path();
        let prev = std::env::var("RUBBERDUX_HOME").ok();
        // Edition 2024 marks env mutation `unsafe`; `ENV_LOCK` keeps it serialized.
        unsafe {
            std::env::set_var("RUBBERDUX_HOME", &home);
        }

        let session_manager = Arc::new(SessionManager {
            home_dir: home.clone(),
            sessions_dir: home.join("sessions"),
            latest_link: home.join("latest"),
        });
        let store: Arc<dyn AppStore> =
            Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));

        // The existing target App the new conversation should merge into, with a
        // stale `last_active` far in the past so any bump is observable.
        let target_id = AppId("2026-06-10-00-00-00-UTC".into());
        let mut target = App::new(
            target_id.clone(),
            "Plan the trip".into(),
            IconSpec {
                symbol: "airplane".into(),
                color: "#3478F6".into(),
            },
            BoardPosition { row: 1, column: 1 },
        );
        target.summary = "plan the tokyo trip itinerary".into();
        target.last_active = "2000-01-01T00:00:00+00:00".into();
        store.create(&target).unwrap();
        // `create` wrote the manifest; ensure the stale timestamp is on disk.
        let target_dir = home.join("apps").join(target_id.as_str());
        std::fs::write(
            target_dir.join("metadata.json"),
            serde_json::to_string(&target).unwrap(),
        )
        .unwrap();

        let supervisor: Arc<dyn DynAppSupervisor> = Arc::new(MemorySupervisor::new(
            dummy_client(),
            session_manager,
            store.clone(),
        ));

        // The just-created App whose conversation we are clustering. Its summary
        // matches the target's exactly, so the offline lexical fallback joins it.
        let new_id = AppId("2026-06-10-00-01-00-UTC".into());
        let mut new_app = target.clone();
        new_app.id = new_id.clone();
        new_app.last_active = chrono::Utc::now().to_rfc3339();
        store.create(&new_app).unwrap();

        let before = store.get(&target_id).unwrap().unwrap().last_active;
        assert_eq!(before, "2000-01-01T00:00:00+00:00");

        consult_clusterer(
            supervisor,
            dummy_client(),
            new_id.clone(),
            "plan the tokyo trip itinerary".into(),
        )
        .await;

        // The target App's `last_active` was bumped and persisted.
        let after = store.get(&target_id).unwrap().unwrap().last_active;
        assert_ne!(after, before, "Join must bump the target's last_active");
        let after_ts = chrono::DateTime::parse_from_rfc3339(&after).unwrap();
        let before_ts = chrono::DateTime::parse_from_rfc3339(&before).unwrap();
        assert!(after_ts > before_ts);

        // The Join was recorded in the target's merge log.
        let merge_log =
            std::fs::read_to_string(target_dir.join("merge_log.jsonl")).unwrap();
        assert!(merge_log.contains("\"join\""));
        assert!(merge_log.contains(new_id.as_str()));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("RUBBERDUX_HOME", v),
                None => std::env::remove_var("RUBBERDUX_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A `New` decision must still be recorded, so every clusterer decision is
    /// auditable. With no candidates, the clusterer chooses `New`; the record is
    /// logged against the new App itself.
    #[tokio::test]
    async fn new_decision_is_recorded_in_merge_log() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let home = tempfile::tempdir().unwrap().into_path();
        let prev = std::env::var("RUBBERDUX_HOME").ok();
        unsafe {
            std::env::set_var("RUBBERDUX_HOME", &home);
        }

        let session_manager = Arc::new(SessionManager {
            home_dir: home.clone(),
            sessions_dir: home.join("sessions"),
            latest_link: home.join("latest"),
        });
        let store: Arc<dyn AppStore> =
            Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));

        // The only App on disk is the new one, so the candidate set is empty and
        // the decision is `New`.
        let new_id = AppId("2026-06-10-00-02-00-UTC".into());
        let new_app = App::new(
            new_id.clone(),
            "Standalone task".into(),
            IconSpec {
                symbol: "doc".into(),
                color: "#8E8E93".into(),
            },
            BoardPosition { row: 2, column: 2 },
        );
        store.create(&new_app).unwrap();

        let supervisor: Arc<dyn DynAppSupervisor> =
            Arc::new(MemorySupervisor::new(dummy_client(), session_manager, store));

        consult_clusterer(
            supervisor,
            dummy_client(),
            new_id.clone(),
            "an entirely unrelated standalone task".into(),
        )
        .await;

        let merge_log = std::fs::read_to_string(
            home.join("apps")
                .join(new_id.as_str())
                .join("merge_log.jsonl"),
        )
        .unwrap();
        assert!(merge_log.contains("\"new\""));
        assert!(merge_log.contains(new_id.as_str()));

        unsafe {
            match prev {
                Some(v) => std::env::set_var("RUBBERDUX_HOME", v),
                None => std::env::remove_var("RUBBERDUX_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
