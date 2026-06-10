//! The interaction raise → respond → resolved round-trip across the gateway
//! board surface (`src/gateway/apps.rs`: `list_interactions` /
//! `respond_interaction`) and the supervisor seam.
//!
//! The **non-live** half, split out by the mock-data policy (root `CLAUDE.md`):
//! the response-routing rules that need no model — responding on an unknown App
//! is 404, and a `request_id` that does not match the path segment is rejected
//! as a missing interaction. These pin the handler's correlation logic
//! deterministically, with no worker subprocess and no model call.
//!
//! The **live, human-gated** positive round-trip — a real `LocalSupervisor`
//! worker (a real `rubberduxd --agent` subprocess driven by the real LLM)
//! raising an interaction that the board lists, answers, and clears — lives in
//! the `app_system` target (`tests/system/app/interaction_round_trip.rs`),
//! whose `main` re-enters as a worker so the supervisor can spawn real
//! subprocesses. The integration harness has no such re-entry.
//!
//! See `docs/agent/interaction.md` and `docs/gateway/apps.md`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use rubberdux::agent::runtime::port::{InputPort, LoopEvent};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::supervisor::MemorySupervisor;
use rubberdux::gateway::apps::router;
use rubberdux::gateway::state::GatewayState;
use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::session::SessionManager;

fn dummy_input_port() -> InputPort {
    let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
    InputPort::new(tx)
}

fn dummy_client() -> Arc<MoonshotClient> {
    Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        "http://127.0.0.1:0".into(),
        "test-key".into(),
        "test-model".into(),
    ))
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

/// Build the board router over an in-process supervisor with a temp store.
fn memory_board_router() -> axum::Router {
    let home = tempfile::tempdir().unwrap().into_path();
    let session_manager = Arc::new(SessionManager {
        home_dir: home.clone(),
        sessions_dir: home.join("sessions"),
        latest_link: home.join("latest"),
    });
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));
    let supervisor = Arc::new(MemorySupervisor::new(dummy_client(), session_manager, store));
    let state = Arc::new(GatewayState::with_apps(
        "sys".into(),
        "id".into(),
        "soul".into(),
        dummy_input_port(),
        supervisor,
        dummy_client(),
    ));
    router().with_state(state)
}

/// Non-live: responding to an interaction on an App that does not exist is a
/// 404, never a generic supervisor 500. Deterministic — no worker, no model.
#[tokio::test(flavor = "multi_thread")]
async fn respond_to_unknown_app_is_not_found() {
    let app = memory_board_router();
    let (status, _) = send(
        &app,
        "POST",
        "/api/v1/apps/1999-01-01-00-00-00-UTC/interactions/req-1",
        Some(serde_json::json!({ "kind": "acknowledged", "request_id": "req-1" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Non-live: a response whose `request_id` does not match the path segment is
/// rejected as a missing interaction, so the correlation between the URL and
/// the body is enforced before the supervisor is touched. Drives a real
/// in-process worker (created App) so the App exists and is Active; the
/// mismatch is what fails the request.
#[tokio::test(flavor = "multi_thread")]
async fn respond_with_mismatched_request_id_is_not_found() {
    let app = memory_board_router();
    let (status, created) = send(
        &app,
        "POST",
        "/api/v1/apps",
        Some(serde_json::json!({
            "task": "anything",
            "position": { "row": 0, "column": 0 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap();

    // The body answers `body-req` but the path addresses `path-req`: a mismatch.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/apps/{id}/interactions/path-req"),
        Some(serde_json::json!({ "kind": "acknowledged", "request_id": "body-req" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
