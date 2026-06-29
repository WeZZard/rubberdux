//! Verifies the host's additive App-board wiring: a gateway state built the way
//! `host::run` builds it — the single-agent surface via
//! `GatewayState::with_trajectory_tx` plus the board surface attached via
//! `GatewayState::attach_apps` over the default subprocess `LocalSupervisor` —
//! serves both the health endpoint and the multi-App board REST surface. Apps
//! load `Tombstoned` at startup (no worker spawned until used), and the existing
//! single-agent routes remain live. See `docs/gateway/apps.md` and
//! `docs/app/runtime/worker-lifecycle.md`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use tower::ServiceExt;

use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::{App, AppId, BoardPosition, IconSpec};
use rubberdux::agent::runtime::port::{InputPort, LoopEvent};
use rubberdux::gateway::route::router;
use rubberdux::gateway::state::{GatewayState, ProviderMeta};

use crate::support::model_api_stub::dummy_model_api;

fn dummy_input_port() -> InputPort {
    let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
    InputPort::new(tx)
}

/// Build the full application router exactly as `gateway::server::run` does: the
/// REST routes from `route::router()` (which merges the App board REST surface)
/// plus the App board WebSocket routes, all bound to the wired state. Mounting
/// the board WS routes confirms they compile against the `attach_apps` state.
fn full_app(state: Arc<GatewayState>) -> axum::Router {
    router()
        .route(
            "/api/v1/ws/apps/{id}/entries",
            get(rubberdux::gateway::apps_stream::ws_app_entries),
        )
        .route(
            "/api/v1/ws/apps/{id}/trajectory",
            get(rubberdux::gateway::apps_stream::ws_app_trajectory),
        )
        .route("/api/v1/ws/board", get(rubberdux::gateway::apps_stream::ws_board))
        .route(
            "/api/v1/ws/apps/{id}/interactions",
            get(rubberdux::gateway::apps_stream::ws_app_interactions),
        )
        .with_state(state)
}

async fn response_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

/// The board surface is wired exactly as `host::run` assembles it: the
/// single-agent state via `with_trajectory_tx`, then `attach_apps` with a
/// `LocalSupervisor` (the default `AppSupervisor`) over a temp filesystem store.
/// Health, the single-agent prompt endpoints, and the App board list must all
/// respond, with the seeded App loaded `Tombstoned`.
#[tokio::test(flavor = "multi_thread")]
async fn host_wired_state_serves_health_and_app_board() {
    // A temp store seeded with one App so `GET /api/v1/apps` has something to
    // return; the store loads it `Tombstoned` at startup (no worker spawned).
    let apps_dir = tempfile::tempdir().unwrap().into_path();
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(apps_dir.clone()));
    let seeded = App::new(
        AppId("2026-06-10-00-00-00-UTC".into()),
        "Plan the trip".into(),
        IconSpec { symbol: "airplane".into(), color: "#3478F6".into() },
        BoardPosition { row: 0, column: 0 },
    );
    store.create(&seeded).unwrap();

    // The default subprocess supervisor: `bind` sets up the `Hello`-routing
    // accept loop, the peer broker, and the idle sweeper. No worker is spawned
    // until an App is used.
    let supervisor = Arc::new(LocalSupervisor::bind(store.clone()).await.unwrap());

    // Build the gateway state the way the host does, then attach the board.
    let (trajectory_tx, _) = tokio::sync::broadcast::channel(16);
    let mut state = GatewayState::with_trajectory_tx(
        "system".into(),
        "identity".into(),
        "soul".into(),
        trajectory_tx,
        dummy_input_port(),
        None,
        dummy_model_api(),
        ProviderMeta {
            provider: "kimi-for-coding".into(),
            model: "test-model".into(),
            dialect: "anthropic-messages".into(),
        },
    );
    // The board list path below never hits the LLM, so a stub adapter is enough
    // for the identity client the board attaches.
    let identity_client = dummy_model_api();
    state.attach_apps(supervisor, identity_client);
    let state = Arc::new(state);

    // The single-agent surface stays live alongside the board.
    let (status, json) = response_json(full_app(state.clone()), "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "ok");

    let (status, json) =
        response_json(full_app(state.clone()), "/api/v1/prompts/system").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["content"], "system");

    // The App board REST surface is live and returns the seeded App, loaded
    // Tombstoned at startup.
    let (status, json) = response_json(full_app(state.clone()), "/api/v1/apps").await;
    assert_eq!(status, StatusCode::OK);
    let apps = json["apps"].as_array().expect("apps array");
    assert_eq!(apps.len(), 1, "the one seeded App must be listed");
    assert_eq!(apps[0]["id"], "2026-06-10-00-00-00-UTC");
    assert_eq!(apps[0]["title"], "Plan the trip");
    assert_eq!(
        apps[0]["status"], "tombstoned",
        "Apps load Tombstoned at startup; no worker spawns until used"
    );
}
