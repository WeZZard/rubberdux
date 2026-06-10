//! The multi-App board WebSocket surface. See `docs/gateway/apps_stream.md`.
//!
//! These handlers are the streaming companion to the REST surface in
//! `super::apps`: per-App entry and trajectory streams, a board-wide lifecycle
//! stream, and a bidirectional per-App interaction stream. Each reuses the same
//! broadcast → serialize → `socket.send(Text)` loop the single-agent surface
//! uses in `super::stream`, with `RecvError::Lagged` (log and resync) and
//! `RecvError::Closed` (end the stream) handling.
//!
//! The interaction signal is gateway-owned: the `AppSupervisor` trait exposes
//! interactions only as a snapshot poll and an answer path, so the live
//! `interaction_raised`/`resolved` events and the board's `badge` are derived
//! from `GatewayState::interaction_tx` rather than by widening the supervisor
//! trait. See `docs/gateway/apps_stream.md` for the rationale.

use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use super::state::GatewayState;
use crate::agent::interaction::{AgentInteraction, InteractionResponse};
use crate::app::supervisor::BoardEvent;
use crate::app::{App, AppId};
use crate::gateway::apps::AppDto;
use crate::trajectory::TrajectoryEvent;

// ---------------------------------------------------------------------------
// Gateway-owned interaction events
// ---------------------------------------------------------------------------

/// A live App interaction lifecycle event the gateway fans out so the board and
/// per-App interaction streams stay live. The supervisor trait has no such
/// event, so the gateway owns it; the raising component publishes through
/// [`GatewayState::publish_interaction`]. See `docs/gateway/apps_stream.md`.
#[derive(Debug, Clone)]
pub enum InteractionEvent {
    /// An App's worker raised an interaction awaiting a human answer.
    Raised(AgentInteraction),
    /// A previously raised interaction was answered and cleared.
    Resolved { app_id: String, request_id: String },
}

// ---------------------------------------------------------------------------
// Wire messages
// ---------------------------------------------------------------------------

/// Outbound frame carrying one App history entry, identical in shape to the
/// single-agent entry frame so the front end reuses one decoder.
#[derive(Serialize)]
struct EntryWsMessage {
    r#type: &'static str,
    entry: crate::agent::entry::Entry,
    is_final: bool,
}

/// Outbound frame carrying one trajectory event.
#[derive(Serialize)]
struct TrajectoryWsMessage {
    r#type: &'static str,
    event: TrajectoryEvent,
}

/// Outbound board-stream frame: the projection of a [`BoardEvent`] plus the
/// gateway-derived `badge`. The `type` tag is the stable wire discriminator,
/// decoupled from the Rust variant name.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BoardWsMessage {
    /// A new App appeared on the board.
    AppCreated { app: AppDto },
    /// An existing App's status or position changed.
    Updated { id: String },
    /// An App was archived and left the board.
    Archived { id: String },
    /// The number of interactions an App is currently awaiting changed.
    Badge { app_id: String, count: usize },
}

/// Outbound per-App interaction frame.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InteractionWsMessage {
    /// An interaction was raised and awaits a human answer.
    InteractionRaised { interaction: AgentInteraction },
    /// A previously raised interaction was answered and cleared.
    Resolved { request_id: String },
}

/// Inbound per-App interaction frame: a human answer to a raised interaction.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InteractionInbound {
    /// Answer a raised interaction with the carried response.
    Respond { response: InteractionResponse },
}

// ---------------------------------------------------------------------------
// Per-App entry / trajectory streams
// ---------------------------------------------------------------------------

/// WebSocket endpoint streaming one App's history entries. Mirrors
/// `super::stream::ws_entries` but sources the receiver from the supervisor's
/// per-App `subscribe_entries`. Closes the socket if the App has no active
/// worker.
pub async fn ws_app_entries(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_app_entries(socket, id, state))
}

async fn handle_app_entries(mut socket: WebSocket, id: String, state: Arc<GatewayState>) {
    let Some(supervisor) = state.supervisor.as_ref() else {
        return;
    };
    let app_id = AppId(id);
    let mut rx = match supervisor.subscribe_entries(&app_id).await {
        Ok(rx) => rx,
        Err(err) => {
            log::warn!("App entries WebSocket: no active worker for `{app_id}`: {err}");
            return;
        }
    };
    loop {
        match rx.recv().await {
            Ok(notification) => {
                let msg = EntryWsMessage {
                    r#type: "entry",
                    entry: notification.entry,
                    is_final: notification.is_final,
                };
                if send_json(&mut socket, &msg).await.is_break() {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                log::warn!("App entries WebSocket receiver lagged by {n} messages");
                continue;
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// WebSocket endpoint streaming one App's trajectory events. Mirrors
/// `super::stream::ws_trajectory` over the supervisor's per-App
/// `subscribe_trajectory`.
pub async fn ws_app_trajectory(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_app_trajectory(socket, id, state))
}

async fn handle_app_trajectory(mut socket: WebSocket, id: String, state: Arc<GatewayState>) {
    let Some(supervisor) = state.supervisor.as_ref() else {
        return;
    };
    let app_id = AppId(id);
    let mut rx = match supervisor.subscribe_trajectory(&app_id).await {
        Ok(rx) => rx,
        Err(err) => {
            log::warn!("App trajectory WebSocket: no active worker for `{app_id}`: {err}");
            return;
        }
    };
    loop {
        match rx.recv().await {
            Ok(event) => {
                let msg = TrajectoryWsMessage {
                    r#type: "trajectory",
                    event,
                };
                if send_json(&mut socket, &msg).await.is_break() {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                log::warn!("App trajectory WebSocket receiver lagged by {n} messages");
                continue;
            }
            Err(RecvError::Closed) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Board stream
// ---------------------------------------------------------------------------

/// WebSocket endpoint streaming board lifecycle events and interaction badges.
/// Multiplexes the supervisor's `subscribe_board()` and the gateway's
/// interaction fan-out in one `tokio::select!` loop, mirroring the
/// bidirectional pattern in `super::stream::ws_chat`.
pub async fn ws_board(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_board(socket, state))
}

async fn handle_board(mut socket: WebSocket, state: Arc<GatewayState>) {
    if state.supervisor.is_none() {
        return;
    }
    let mut board_rx = state.board_tx.subscribe();
    let mut interaction_rx = state.interaction_tx.subscribe();
    loop {
        tokio::select! {
            event = board_rx.recv() => {
                match event {
                    Ok(event) => {
                        let msg = project_board_event(event);
                        if send_json(&mut socket, &msg).await.is_break() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        log::warn!("Board WebSocket board receiver lagged by {n} messages");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            interaction = interaction_rx.recv() => {
                match interaction {
                    Ok(InteractionEvent::Raised(raised)) => {
                        // A raised interaction lights the App's attention badge.
                        let msg = BoardWsMessage::Badge {
                            app_id: raised.app_id().to_owned(),
                            count: 1,
                        };
                        if send_json(&mut socket, &msg).await.is_break() {
                            break;
                        }
                    }
                    Ok(InteractionEvent::Resolved { app_id, .. }) => {
                        // The interaction cleared; the badge returns to zero.
                        let msg = BoardWsMessage::Badge { app_id, count: 0 };
                        if send_json(&mut socket, &msg).await.is_break() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        log::warn!("Board WebSocket interaction receiver lagged by {n} messages");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Translate a supervisor [`BoardEvent`] into its stable board wire message.
/// `StatusChanged` and `Moved` both collapse to `updated`: the board reloads
/// the affected App's view rather than diffing field-level changes.
fn project_board_event(event: BoardEvent) -> BoardWsMessage {
    match event {
        BoardEvent::Created(app) => BoardWsMessage::AppCreated {
            app: app_to_dto(app),
        },
        BoardEvent::StatusChanged { id, .. } => BoardWsMessage::Updated { id: id.0 },
        BoardEvent::Moved { id, .. } => BoardWsMessage::Updated { id: id.0 },
        BoardEvent::Archived(id) => BoardWsMessage::Archived { id: id.0 },
    }
}

/// Project a domain [`App`] to its wire DTO. The freshly created App carried by
/// a `Created` event is already `Active`; the REST DTO conversion preserves it.
fn app_to_dto(app: App) -> AppDto {
    AppDto::from(app)
}

// ---------------------------------------------------------------------------
// Per-App interaction stream
// ---------------------------------------------------------------------------

/// Bidirectional WebSocket endpoint for one App's interactions. Outbound it
/// emits `interaction_raised`/`resolved` derived from the gateway's interaction
/// fan-out filtered to this App; inbound it accepts a `respond` message,
/// answers the interaction through the supervisor, and publishes a `Resolved`
/// event so every subscriber sees it clear. Mirrors the `tokio::select!` shape
/// of `super::stream::ws_chat`.
pub async fn ws_app_interactions(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_app_interactions(socket, id, state))
}

async fn handle_app_interactions(mut socket: WebSocket, id: String, state: Arc<GatewayState>) {
    let Some(supervisor) = state.supervisor.as_ref() else {
        return;
    };
    let app_id = AppId(id);
    let mut interaction_rx = state.interaction_tx.subscribe();

    // Replay the interactions this App is already awaiting so a client that
    // connects after they were raised still sees them.
    match supervisor.pending_interactions(&app_id).await {
        Ok(pending) => {
            for interaction in pending {
                let msg = InteractionWsMessage::InteractionRaised { interaction };
                if send_json(&mut socket, &msg).await.is_break() {
                    return;
                }
            }
        }
        Err(err) => {
            log::warn!("App interactions WebSocket: no active worker for `{app_id}`: {err}");
            return;
        }
    }

    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(WsMessage::Text(text))) => {
                        match serde_json::from_str::<InteractionInbound>(&text) {
                            Ok(InteractionInbound::Respond { response }) => {
                                let request_id = response.request_id().to_owned();
                                if let Err(err) =
                                    supervisor.respond_to_interaction(&app_id, response).await
                                {
                                    log::warn!(
                                        "App interactions WebSocket: respond failed for `{app_id}`: {err}"
                                    );
                                    continue;
                                }
                                // Announce the resolution to every subscriber,
                                // including the board's badge.
                                state.publish_interaction(InteractionEvent::Resolved {
                                    app_id: app_id.0.clone(),
                                    request_id,
                                });
                            }
                            Err(err) => {
                                log::warn!("Invalid JSON on interaction WebSocket: {err}");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Err(e)) => {
                        log::warn!("App interactions WebSocket receive error: {e}");
                        break;
                    }
                    _ => continue,
                }
            }
            event = interaction_rx.recv() => {
                match event {
                    Ok(InteractionEvent::Raised(raised)) if raised.app_id() == app_id.0 => {
                        let msg = InteractionWsMessage::InteractionRaised { interaction: raised };
                        if send_json(&mut socket, &msg).await.is_break() {
                            break;
                        }
                    }
                    Ok(InteractionEvent::Resolved { app_id: ev_app, request_id })
                        if ev_app == app_id.0 =>
                    {
                        let msg = InteractionWsMessage::Resolved { request_id };
                        if send_json(&mut socket, &msg).await.is_break() {
                            break;
                        }
                    }
                    // An event for a different App; ignore it.
                    Ok(_) => continue,
                    Err(RecvError::Lagged(n)) => {
                        log::warn!("App interactions WebSocket receiver lagged by {n} messages");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize a wire message and send it as a text frame. Returns
/// [`std::ops::ControlFlow::Break`] when the socket is gone (the caller should
/// stop streaming) and `Continue` otherwise; a serialization failure is logged
/// and treated as `Continue` so one bad frame does not drop the connection.
async fn send_json<T: Serialize>(socket: &mut WebSocket, msg: &T) -> std::ops::ControlFlow<()> {
    let json = match serde_json::to_string(msg) {
        Ok(j) => j,
        Err(err) => {
            log::warn!("Failed to serialize WebSocket message: {err}");
            return std::ops::ControlFlow::Continue(());
        }
    };
    if socket.send(WsMessage::Text(json.into())).await.is_err() {
        std::ops::ControlFlow::Break(())
    } else {
        std::ops::ControlFlow::Continue(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    use crate::agent::interaction::ApprovalFlavor;
    use crate::agent::runtime::port::{InputPort, LoopEvent};
    use crate::app::registry::store::{AppStore, FilesystemAppStore};
    use crate::app::supervisor::{AppSupervisor, CreateAppRequest, MemorySupervisor};
    use crate::app::{BoardPosition, IconSpec};
    use crate::provider::moonshot::MoonshotClient;
    use crate::session::SessionManager;

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

    /// Build the board WebSocket router over a real `MemorySupervisor` and serve
    /// it on an ephemeral loopback port, returning the bound address and the
    /// shared state so a test can both connect a client and drive the supervisor
    /// in-process.
    async fn serve() -> (std::net::SocketAddr, Arc<GatewayState>, Arc<MemorySupervisor>) {
        let supervisor = memory_supervisor();
        let state = Arc::new(GatewayState::with_apps(
            "sys".into(),
            "id".into(),
            "soul".into(),
            dummy_input_port(),
            supervisor.clone(),
            dummy_client(),
        ));

        let app = axum::Router::new()
            .route("/api/v1/ws/board", axum::routing::get(ws_board))
            .route(
                "/api/v1/ws/apps/{id}/interactions",
                axum::routing::get(ws_app_interactions),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, state, supervisor)
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

    /// Read text frames until one parses as JSON satisfying `predicate`, or time
    /// out. Returns the matching value.
    async fn recv_until<S, F>(stream: &mut S, predicate: F) -> serde_json::Value
    where
        S: StreamExt<Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
        F: Fn(&serde_json::Value) -> bool,
    {
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(msg)) = stream.next().await {
                if let ClientMessage::Text(text) = msg {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if predicate(&value) {
                            return Some(value);
                        }
                    }
                }
            }
            None
        })
        .await
        .unwrap_or(None);
        found.expect("expected a matching WebSocket frame before timeout")
    }

    #[tokio::test]
    async fn board_stream_emits_app_created_and_interaction_badge() {
        let (addr, state, supervisor) = serve().await;

        let url = format!("ws://{addr}/api/v1/ws/board");
        let (mut socket, _) = connect_async(url).await.unwrap();

        // Creating an App through the supervisor must surface as `app_created`.
        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        let created = recv_until(&mut socket, |v| v["type"] == "app_created").await;
        assert_eq!(created["app"]["id"], app.id.0);

        // A raised interaction must light the App's badge on the board stream.
        let interaction = AgentInteraction::Approval {
            request_id: "r1".into(),
            app_id: app.id.0.clone(),
            flavor: ApprovalFlavor::Permission,
            prompt: "delete file".into(),
        };
        state.publish_interaction(InteractionEvent::Raised(interaction));

        let badge = recv_until(&mut socket, |v| v["type"] == "badge").await;
        assert_eq!(badge["app_id"], app.id.0);
        assert_eq!(badge["count"], 1);
    }

    #[tokio::test]
    async fn interaction_stream_round_trips_raise_respond_resolved() {
        let (addr, state, supervisor) = serve().await;

        let app = supervisor
            .create_app(sample_request(), "start planning".into())
            .await
            .unwrap();

        let url = format!("ws://{addr}/api/v1/ws/apps/{}/interactions", app.id.0);
        let (mut socket, _) = connect_async(url).await.unwrap();

        // Raise an interaction for this App; the per-App stream must surface it.
        let interaction = AgentInteraction::Approval {
            request_id: "req-42".into(),
            app_id: app.id.0.clone(),
            flavor: ApprovalFlavor::Permission,
            prompt: "approve plan".into(),
        };
        state.publish_interaction(InteractionEvent::Raised(interaction));

        let raised = recv_until(&mut socket, |v| v["type"] == "interaction_raised").await;
        assert_eq!(raised["interaction"]["request_id"], "req-42");

        // Answer it over the same socket; the handler resolves it through the
        // supervisor and re-broadcasts a `resolved` event back to us.
        let response = InteractionResponse::Approved {
            request_id: "req-42".into(),
            flavor: ApprovalFlavor::Permission,
        };
        let inbound = serde_json::json!({ "type": "respond", "response": response });
        socket
            .send(ClientMessage::Text(inbound.to_string().into()))
            .await
            .unwrap();

        let resolved = recv_until(&mut socket, |v| v["type"] == "resolved").await;
        assert_eq!(resolved["request_id"], "req-42");
    }
}
