use std::sync::Arc;

use super::error::GatewayError;
use super::state::GatewayState;

pub async fn run(state: Arc<GatewayState>) -> Result<(), GatewayError> {
    let port: u16 = std::env::var("RUBBERDUX_GATEWAY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(19385);

    // `route::router()` already merges the multi-App board REST surface
    // (`super::apps::router()`) alongside the single-agent endpoints; the
    // WebSocket routes are layered on here. See `docs/gateway/apps.md`.
    let app = super::route::router()
        .route(
            "/api/v1/ws/entries",
            axum::routing::get(super::stream::ws_entries),
        )
        .route(
            "/api/v1/ws/trajectory",
            axum::routing::get(super::stream::ws_trajectory),
        )
        .route(
            "/api/v1/ws/chat",
            axum::routing::get(super::stream::ws_chat),
        )
        // Multi-App board WebSocket surface, mounted additively alongside the
        // single-agent streams above. See `docs/gateway/apps_stream.md`.
        .route(
            "/api/v1/ws/apps/{id}/entries",
            axum::routing::get(super::apps_stream::ws_app_entries),
        )
        .route(
            "/api/v1/ws/apps/{id}/trajectory",
            axum::routing::get(super::apps_stream::ws_app_trajectory),
        )
        .route(
            "/api/v1/ws/board",
            axum::routing::get(super::apps_stream::ws_board),
        )
        .route(
            "/api/v1/ws/apps/{id}/interactions",
            axum::routing::get(super::apps_stream::ws_app_interactions),
        )
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    log::info!("Gateway server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
