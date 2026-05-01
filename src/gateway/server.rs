use std::sync::Arc;

use super::error::GatewayError;
use super::state::GatewayState;

pub async fn run(state: Arc<GatewayState>) -> Result<(), GatewayError> {
    let port: u16 = std::env::var("RUBBERDUX_GATEWAY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(19385);

    let app = super::route::router()
        .route(
            "/api/v1/ws/entries",
            axum::routing::get(super::stream::ws_entries),
        )
        .route(
            "/api/v1/ws/trajectory",
            axum::routing::get(super::stream::ws_trajectory),
        )
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    log::info!("Gateway server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
