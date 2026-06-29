//! Integration coverage for the multi-App gateway REST surface
//! (`src/gateway/apps.rs`) driven over a real [`MemorySupervisor`] — the
//! in-process realization of the [`AppSupervisor`] seam.
//!
//! `app_board_wiring.rs` proves the host assembles one seeded App over a
//! subprocess `LocalSupervisor`; this test exercises the *multi-App* board
//! lifecycle through the HTTP boundary with two concurrent Apps over the
//! in-process supervisor: create two, list both, fetch each by id, move one,
//! archive one (and confirm it leaves the default listing while the other
//! stays), then restore the archived id back to addressable. No live LLM is
//! needed: the dummy client points at an unreachable URL and these routes —
//! create/list/get/patch/archive — never drive an LLM turn. See
//! `docs/gateway/apps.md`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use rubberdux::agent::runtime::port::{InputPort, LoopEvent};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::supervisor::MemorySupervisor;
use rubberdux::app::{App, AppId, BoardPosition, IconSpec};
use rubberdux::gateway::apps::router;
use rubberdux::gateway::state::{GatewayState, ProviderMeta};
use rubberdux::provider::ModelApi;
use rubberdux::session::SessionManager;

use crate::support::model_api_stub::openai_model_api;

fn dummy_input_port() -> InputPort {
    let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
    InputPort::new(tx)
}

/// A client whose base URL is unreachable: the board lifecycle routes under
/// test never reach the network, and the background identity/clustering tasks
/// `create_app` spawns are best-effort and tolerate a failed call.
fn dummy_client() -> Arc<dyn ModelApi> {
    openai_model_api("http://127.0.0.1:0", "test-model")
}

/// Provider identity snapshot for the board state under test; the routes
/// exercised never read it, so any consistent stub serves.
fn dummy_provider_meta() -> ProviderMeta {
    ProviderMeta {
        provider: "kimi-for-coding".into(),
        model: "test-model".into(),
        dialect: "anthropic-messages".into(),
    }
}

/// Build the App board router over a fresh in-process supervisor rooted under a
/// temp home, returning the router plus the underlying store so a test can seed
/// Apps with explicit, distinct ids. The store loads seeded Apps `Tombstoned`
/// (no worker until used), exactly as the host does at startup.
fn board_router_with_store() -> (axum::Router, Arc<dyn AppStore>) {
    let home = tempfile::tempdir().unwrap().into_path();
    let session_manager = Arc::new(SessionManager {
        home_dir: home.clone(),
        sessions_dir: home.join("sessions"),
        latest_link: home.join("latest"),
    });
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));
    let supervisor = Arc::new(MemorySupervisor::new(
        dummy_client(),
        session_manager,
        store.clone(),
    ));
    let state = Arc::new(GatewayState::with_apps(
        "sys".into(),
        "id".into(),
        "soul".into(),
        dummy_input_port(),
        supervisor,
        dummy_client(),
        dummy_client(),
        dummy_provider_meta(),
    ));
    (router().with_state(state), store)
}

fn board_router() -> axum::Router {
    board_router_with_store().0
}

/// Seed an App with an explicit id directly into the store. Used to place two
/// distinct Apps on the board without relying on `AppId::now()` (which is
/// second-granularity and would collide for two creates in the same second).
fn seed_app(store: &Arc<dyn AppStore>, id: &str, title: &str, row: i32, column: i32) {
    let app = App::new(
        AppId(id.into()),
        title.into(),
        IconSpec { symbol: "doc".into(), color: "#8E8E93".into() },
        BoardPosition { row, column },
    );
    store.create(&app).unwrap();
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

/// A single real `create_app` returns 201 immediately with an Active App. (The
/// in-depth create/patch/archive unit coverage lives in `src/gateway/apps.rs`;
/// this confirms the route is reachable through the integration crate's wiring.)
#[tokio::test]
async fn create_returns_active_app_immediately() {
    let app = board_router();
    let (status, created) = send(
        &app,
        "POST",
        "/api/v1/apps",
        Some(serde_json::json!({
            "task": "Plan the Tokyo trip",
            "position": { "row": 0, "column": 0 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create must return 201 (body: {created})");
    assert_eq!(created["status"], "active");
    assert!(created["id"].as_str().is_some(), "create returns the new App id");
}

/// Two distinct Apps coexist on the board: both are listed, each is fetchable
/// by id, one can be moved, and archiving one removes only it from the default
/// listing while the other stays — then the archived id restores back Active.
///
/// The two Apps are seeded with explicit distinct ids so the test does not
/// depend on `AppId::now()` minting two different ids within the same second.
/// Seeded Apps load `Tombstoned` (the store never reports a worker as live);
/// `restore` is what spawns a worker and flips one Active.
#[tokio::test(flavor = "multi_thread")]
async fn two_apps_board_lifecycle_over_memory_supervisor() {
    let (app, store) = board_router_with_store();

    let first = "2026-06-10-00-00-00-UTC";
    let second = "2026-06-10-00-00-01-UTC";
    seed_app(&store, first, "Plan the Tokyo trip", 0, 0);
    seed_app(&store, second, "Refactor the billing module", 1, 0);

    // The board lists both seeded Apps, each Tombstoned (no worker until used).
    let (status, list) = send(&app, "GET", "/api/v1/apps", None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "both Apps are on the board");
    assert!(ids.contains(&first));
    assert!(ids.contains(&second));

    // Each is fetchable by id.
    for id in [first, second] {
        let (status, got) = send(&app, "GET", &format!("/api/v1/apps/{id}"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["id"], id);
        assert_eq!(got["status"], "tombstoned");
    }

    // Move the first App; the patch returns its current view with 200.
    let (status, _) = send(
        &app,
        "PATCH",
        &format!("/api/v1/apps/{first}"),
        Some(serde_json::json!({ "position": { "row": 3, "column": 4 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Archive the second App: it leaves the default listing, the first stays.
    let (status, _) = send(&app, "DELETE", &format!("/api/v1/apps/{second}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, list) = send(&app, "GET", "/api/v1/apps", None).await;
    assert_eq!(status, StatusCode::OK);
    let remaining: Vec<&str> = list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(remaining, vec![first], "only the unarchived App lists");

    // The archived App is still addressable by id (Tombstoned), and restoring
    // it brings a worker back Active.
    let (status, got) = send(&app, "GET", &format!("/api/v1/apps/{second}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["status"], "tombstoned");

    let (status, restored) =
        send(&app, "POST", &format!("/api/v1/apps/{second}/restore"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored["status"], "active", "restore brings the worker back");
}

/// Unknown App ids are 404 across the board surface — the multi-App routing
/// rejects an id that was never created without leaking it as a 500.
#[tokio::test(flavor = "multi_thread")]
async fn unknown_app_id_is_not_found_across_routes() {
    let app = board_router();
    let missing = "1999-01-01-00-00-00-UTC";

    for (method, suffix, body) in [
        ("GET", "", None),
        ("DELETE", "", None),
        ("POST", "/restore", None),
        (
            "POST",
            "/tasks",
            Some(serde_json::json!({ "text": "hi" })),
        ),
        ("GET", "/interactions", None),
    ] {
        let (status, _) = send(
            &app,
            method,
            &format!("/api/v1/apps/{missing}{suffix}"),
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} /api/v1/apps/{{id}}{suffix} on an unknown id must be 404"
        );
    }
}

/// Test that sequential App creation (with monotonic counter AppIds) produces
/// distinct IDs and all apps persist.
#[tokio::test(flavor = "multi_thread")]
async fn sequential_creates_all_persist_with_distinct_ids() {
    let app = board_router();

    // Create N apps sequentially.
    let n = 5;
    let mut ids = vec![];

    for i in 0..n {
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/apps",
            Some(serde_json::json!({
                "task": format!("App {}", i),
                "position": { "row": 0, "column": i }
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create #{} must return 201, got {} with body: {}",
            i,
            status,
            serde_json::to_string(&body).unwrap_or_else(|_| "??".into())
        );
        ids.push(body["id"].as_str().unwrap().to_string());
    }

    // All N ids must be distinct.
    let unique_ids: std::collections::HashSet<_> = ids.iter().cloned().collect();
    assert_eq!(
        unique_ids.len(),
        n,
        "all {} ids must be distinct; got: {:?}",
        n,
        ids
    );

    // All N apps must be listed as active.
    let (status, list) = send(&app, "GET", "/api/v1/apps", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed_ids: Vec<&str> = list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed_ids.len(),
        n,
        "all {} apps must be listed; only {} were persisted",
        n,
        listed_ids.len()
    );
}

/// Test that concurrent AppId::now() calls produce distinct IDs when used
/// across multiple threads.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_app_ids_are_distinct() {
    let n = 10;
    let mut tasks = vec![];

    // Spawn tasks that create AppIds concurrently.
    for _ in 0..n {
        let task = tokio::spawn(async move {
            rubberdux::app::AppId::now()
        });
        tasks.push(task);
    }

    let mut ids = vec![];
    for task in tasks {
        ids.push(task.await.unwrap());
    }

    // All IDs must be distinct.
    let unique_ids: std::collections::HashSet<_> = ids.iter().cloned().collect();
    assert_eq!(
        unique_ids.len(),
        n,
        "all {} AppIds must be distinct; got: {:?}",
        n,
        ids.iter().map(|id| id.as_str()).collect::<Vec<_>>()
    );
}

/// Two Apps created at the SAME board cell both persist with distinct ids. The
/// gateway does not enforce board-position uniqueness, so a cell may hold more
/// than one App; clients render the overlap stacked. This pins the same-cell
/// placement behavior documented in `docs/gateway/apps.md`.
#[tokio::test(flavor = "multi_thread")]
async fn same_cell_creates_both_persist() {
    let app = board_router();

    let mut ids = vec![];
    for label in ["First", "Second"] {
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/apps",
            Some(serde_json::json!({
                "task": format!("{label} at the shared cell"),
                "position": { "row": 2, "column": 2 }
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "same-cell create must return 201, got {} with body: {}",
            status,
            serde_json::to_string(&body).unwrap_or_else(|_| "??".into())
        );
        ids.push(body["id"].as_str().unwrap().to_string());
    }

    // The shared cell does not collapse the two Apps onto one id.
    assert_ne!(ids[0], ids[1], "same-cell creates still mint distinct ids");

    // Both Apps persist on the board, each still reporting the shared cell.
    let (status, list) = send(&app, "GET", "/api/v1/apps", None).await;
    assert_eq!(status, StatusCode::OK);
    let at_cell: Vec<&serde_json::Value> = list["apps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| ids.iter().any(|id| id == a["id"].as_str().unwrap()))
        .collect();
    assert_eq!(
        at_cell.len(),
        2,
        "both same-cell Apps persist on the board; got {}",
        at_cell.len()
    );
    for a in at_cell {
        assert_eq!(a["position"]["row"], 2, "same-cell App keeps its row");
        assert_eq!(a["position"]["column"], 2, "same-cell App keeps its column");
    }
}
