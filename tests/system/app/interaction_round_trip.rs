//! System case: the positive interaction raise → respond → resolved
//! round-trip over a real subprocess worker.
//!
//! Wires the gateway board (`src/gateway/apps.rs`) over a real
//! [`LocalSupervisor`] (which spawns a `rubberduxd --agent` subprocess,
//! re-entered as a worker by this target's `main`). Creates an App whose task
//! asks the agent to request explicit approval before doing anything, polls
//! `GET /interactions` until the worker raises one, answers it over REST
//! (expecting 204), then confirms the pending set clears.
//!
//! This lives in the `app_system` target rather than the integration target
//! because it spawns a real worker subprocess: the supervisor re-invokes
//! `current_exe --agent …`, which only this target's `main` re-enters as a
//! worker. Per the mock-data policy (root `CLAUDE.md`) it uses the real model,
//! so it is gated on live-LLM credentials.
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
use rubberdux::gateway::apps::router;
use rubberdux::gateway::state::GatewayState;
use rubberdux::provider::moonshot::MoonshotClient;

use crate::live_gate::skip_without_live_llm;

fn dummy_input_port() -> InputPort {
    let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
    InputPort::new(tx)
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

pub async fn run() {
    if skip_without_live_llm("interaction_round_trip") {
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
