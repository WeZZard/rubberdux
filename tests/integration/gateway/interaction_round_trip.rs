//! The interaction raise → respond → resolved round-trip across the gateway
//! board surface (`src/gateway/apps.rs`: `list_interactions` /
//! `respond_interaction`) and the supervisor seam.
//!
//! Two halves, split by the mock-data policy (root `CLAUDE.md`):
//!
//! - **Non-live, always run:** the response-routing rules that need no model —
//!   responding on an unknown App is 404, and a `request_id` that does not
//!   match the path segment is rejected as a missing interaction. These pin the
//!   handler's correlation logic deterministically.
//! - **Live, human-gated:** the positive round-trip. A real `LocalSupervisor`
//!   worker (a real `rubberduxd --agent` subprocess driven by the real LLM)
//!   raises an interaction; the board lists it, answers it over REST, and the
//!   pending set then clears. This needs live credentials + a host environment,
//!   so it skips with a clear message when `RUBBERDUX_LLM_API_KEY` is absent.
//!
//! See `docs/agent/interaction.md` and `docs/gateway/apps.md`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use rubberdux::agent::runtime::port::{InputPort, LoopEvent};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::supervisor::MemorySupervisor;
use rubberdux::gateway::apps::router;
use rubberdux::gateway::state::GatewayState;
use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::session::SessionManager;

use crate::support::live_gate::skip_without_live_llm;

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

/// LIVE (human-gated): the positive raise → respond → resolved round-trip over a
/// real subprocess worker.
///
/// Wires the board over a `LocalSupervisor`, creates an App whose task asks the
/// agent to request explicit approval before doing anything, polls
/// `GET /interactions` until the worker raises one, answers it over REST
/// (expecting 204), then confirms the pending set is empty afterward.
///
/// Requires `RUBBERDUX_LLM_API_KEY` (+ a host that can spawn
/// `rubberduxd --agent`); skips cleanly otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn live_interaction_round_trip_over_local_supervisor() {
    if skip_without_live_llm("live_interaction_round_trip_over_local_supervisor") {
        return;
    }

    let home = tempfile::tempdir().unwrap().into_path();
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));
    let supervisor = Arc::new(LocalSupervisor::bind(store).await.unwrap());
    let identity_client = Arc::new(MoonshotClient::from_env());
    let state = Arc::new(GatewayState::with_apps(
        "You are a careful assistant. Before taking any action, raise an \
         approval interaction and wait for the answer."
            .into(),
        "id".into(),
        "soul".into(),
        dummy_input_port(),
        supervisor,
        identity_client,
    ));
    let app = router().with_state(state);

    let (status, created) = send(
        &app,
        "POST",
        "/api/v1/apps",
        Some(serde_json::json!({
            "task": "Ask me for explicit approval before you proceed with anything.",
            "position": { "row": 0, "column": 0 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();

    // Poll until the worker raises at least one interaction.
    let request_id = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let (status, body) =
                send(&app, "GET", &format!("/api/v1/apps/{id}/interactions"), None).await;
            assert_eq!(status, StatusCode::OK);
            if let Some(rid) = body["interactions"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|first| first["request_id"].as_str())
            {
                return rid.to_owned();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .expect("the live worker must raise an interaction within the timeout");

    // Answer it over REST. Acknowledged is accepted for any flavor; for an
    // Approval the worker also accepts it as a grant signal.
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/v1/apps/{id}/interactions/{request_id}"),
        Some(serde_json::json!({ "kind": "acknowledged", "request_id": request_id })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "answering returns 204");

    // The answered interaction clears from the pending set.
    let cleared = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (_, body) =
                send(&app, "GET", &format!("/api/v1/apps/{id}/interactions"), None).await;
            let still_pending = body["interactions"]
                .as_array()
                .map(|a| a.iter().any(|i| i["request_id"] == request_id))
                .unwrap_or(false);
            if !still_pending {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(cleared, "the answered interaction must clear from the pending set");
}
