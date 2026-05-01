use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use super::state::GatewayState;
use crate::agent::entry::Entry;
use crate::agent::runtime::port::{EntryNotification, OutputPort};
use crate::trajectory::TrajectoryEvent;

#[derive(Serialize)]
struct EntryWsMessage {
    r#type: &'static str,
    entry: Entry,
    is_final: bool,
}

#[derive(Serialize)]
struct TrajectoryWsMessage {
    r#type: &'static str,
    event: TrajectoryEvent,
}

/// Receives entries from an agent loop output port and mirrors them
/// into the shared gateway state, re-broadcasting each notification
/// so that connected WebSocket clients can observe them.
pub async fn mirror_entries(mut output_port: OutputPort, state: Arc<GatewayState>) {
    while let Some(notification) = output_port.recv().await {
        state.entries.write().await.push(notification.entry.clone());
        if let Err(err) = state.entry_tx.send(notification) {
            log::warn!("Failed to broadcast entry notification: {err}");
        }
    }
}

/// WebSocket endpoint that streams `EntryNotification` events to
/// connected clients as JSON text frames.
pub async fn ws_entries(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let rx = state.entry_tx.subscribe();
    ws.on_upgrade(move |socket| handle_entry_socket(socket, rx))
}

async fn handle_entry_socket(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<EntryNotification>,
) {
    loop {
        match rx.recv().await {
            Ok(notification) => {
                let msg = EntryWsMessage {
                    r#type: "entry",
                    entry: notification.entry,
                    is_final: notification.is_final,
                };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(err) => {
                        log::warn!("Failed to serialize entry message: {err}");
                        continue;
                    }
                };
                if socket.send(WsMessage::Text(json.into())).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                log::warn!("Entry WebSocket receiver lagged by {n} messages");
                continue;
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// WebSocket endpoint that streams `TrajectoryEvent` events to
/// connected clients as JSON text frames.
pub async fn ws_trajectory(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    let rx = state.trajectory_tx.subscribe();
    ws.on_upgrade(move |socket| handle_trajectory_socket(socket, rx))
}

async fn handle_trajectory_socket(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<TrajectoryEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                let msg = TrajectoryWsMessage {
                    r#type: "trajectory",
                    event,
                };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(err) => {
                        log::warn!("Failed to serialize trajectory message: {err}");
                        continue;
                    }
                };
                if socket.send(WsMessage::Text(json.into())).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                log::warn!("Trajectory WebSocket receiver lagged by {n} messages");
                continue;
            }
            Err(RecvError::Closed) => break,
        }
    }
}
