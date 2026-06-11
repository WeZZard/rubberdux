//! Integration coverage for persisting an App's background-derived identity
//! (`src/gateway/apps.rs` create handler + `AppStore::set_identity` +
//! `BoardEvent::IdentityChanged`). When an App is created, the derivation task
//! must overwrite the heuristic placeholder identity on disk and announce an
//! `updated` board event so subscribers refresh — proving `GET /api/v1/apps`
//! serves the derived title/icon, not the placeholder.
//!
//! Two tests cover the path:
//!
//! - `set_identity_is_persisted_and_idempotent` is a deterministic store-layer
//!   check (no LLM): a known identity overwrites the placeholder in the manifest
//!   and a second identical write is idempotent.
//! - `create_persists_derived_identity_and_broadcasts_updated` drives the real
//!   create handler with a live `MoonshotClient` (per the mock-data policy,
//!   integration tests use real model calls): it connects a board WebSocket,
//!   creates an App, and asserts the WS receives an `updated` frame and that
//!   `GET /apps` reflects the persisted, derived identity rather than the
//!   placeholder seed.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use futures::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

use rubberdux::agent::runtime::port::{InputPort, LoopEvent};
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::supervisor::MemorySupervisor;
use rubberdux::app::{App, AppId, BoardPosition, IconSpec};
use rubberdux::gateway::apps::router;
use rubberdux::gateway::apps_stream::ws_board;
use rubberdux::gateway::state::GatewayState;
use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::session::SessionManager;

/// The placeholder identity the create handler seeds before derivation, per
/// `src/gateway/apps.rs`. The test asserts the persisted identity is no longer
/// this.
const PLACEHOLDER_SYMBOL: &str = "doc";
const PLACEHOLDER_COLOR: &str = "#8E8E93";

/// Serializes tests that mutate the process-global `RUBBERDUX_HOME` so the
/// create handler's `FilesystemAppStore::new()` resolves to the same temp home
/// its supervisor uses, without racing other tests on the env var.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn dummy_input_port() -> InputPort {
    let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
    InputPort::new(tx)
}

#[tokio::test]
async fn set_identity_is_persisted_and_idempotent() {
    let home = tempfile::tempdir().unwrap().into_path();
    let store = FilesystemAppStore::with_apps_dir(home.join("apps"));

    // Create an App carrying the heuristic placeholder identity, exactly as the
    // create handler seeds it.
    let id = AppId("2026-06-10-00-00-00-UTC".into());
    let app = App::new(
        id.clone(),
        "raw task text".into(),
        IconSpec { symbol: PLACEHOLDER_SYMBOL.into(), color: PLACEHOLDER_COLOR.into() },
        BoardPosition { row: 0, column: 0 },
    );
    store.create(&app).unwrap();

    // Overwrite the placeholder with a derived identity.
    let icon = IconSpec { symbol: "airplane".into(), color: "#3478F6".into() };
    store.set_identity(&id, "Plan the Tokyo trip", &icon, "trip planning").unwrap();

    let after = store.get(&id).unwrap().unwrap();
    assert_eq!(after.title, "Plan the Tokyo trip");
    assert_eq!(after.icon.symbol, "airplane");
    assert_eq!(after.icon.color, "#3478F6");
    assert_eq!(after.summary, "trip planning");
    // The other fields are untouched.
    assert_eq!(after.position, BoardPosition { row: 0, column: 0 });

    // A second identical write leaves the manifest identity unchanged.
    store.set_identity(&id, "Plan the Tokyo trip", &icon, "trip planning").unwrap();
    let again = store.get(&id).unwrap().unwrap();
    assert_eq!(again, after, "set_identity is idempotent");

    let _ = std::fs::remove_dir_all(&home);
}

/// Build the board REST router plus the board WebSocket over a real in-process
/// supervisor rooted at `apps_dir`, and serve it on an ephemeral loopback port.
/// Returns the bound address and the served `axum::Router` clone for `oneshot`
/// REST calls against the same state. The real `MoonshotClient` drives the
/// background identity derivation the create handler spawns.
async fn serve(apps_dir: std::path::PathBuf, client: Arc<MoonshotClient>) -> std::net::SocketAddr {
    let home = apps_dir.parent().unwrap().to_path_buf();
    let session_manager = Arc::new(SessionManager {
        home_dir: home.clone(),
        sessions_dir: home.join("sessions"),
        latest_link: home.join("latest"),
    });
    let store: Arc<dyn AppStore> = Arc::new(FilesystemAppStore::with_apps_dir(apps_dir));
    let supervisor = Arc::new(MemorySupervisor::new(
        client.clone(),
        session_manager,
        store,
    ));
    let state = Arc::new(GatewayState::with_apps(
        "sys".into(),
        "id".into(),
        "soul".into(),
        dummy_input_port(),
        supervisor,
        client,
    ));

    let app = router()
        .merge(
            axum::Router::new()
                .route("/api/v1/ws/board", axum::routing::get(ws_board)),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let url = format!("http://{addr}{uri}");
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let client = reqwest::Client::new();
    let mut builder = client.request(method, &url);
    if let Some(value) = body {
        builder = builder.json(&value);
    }
    let resp = builder.send().await.unwrap();
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
    let bytes = resp.bytes().await.unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Read text frames until one parses as JSON satisfying `predicate`, or time
/// out. Returns the matching value.
async fn recv_until<S, F>(stream: &mut S, predicate: F) -> Option<serde_json::Value>
where
    S: StreamExt<Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
    F: Fn(&serde_json::Value) -> bool,
{
    tokio::time::timeout(Duration::from_secs(60), async {
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
    .unwrap_or(None)
}

/// The end-to-end create path: a real `create_app` over the in-process
/// supervisor must derive an identity, persist it over the placeholder, and
/// announce an `updated` board event so `GET /apps` serves the derived
/// title/icon. Uses a live `MoonshotClient` from the environment per the
/// mock-data policy (load `.env` before running).
#[tokio::test(flavor = "multi_thread")]
async fn create_persists_derived_identity_and_broadcasts_updated() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // The create handler's derivation task resolves `RUBBERDUX_HOME` via
    // `FilesystemAppStore::new()`; point it at the same temp home the served
    // supervisor's store roots under so both see the same Apps on disk.
    let home = tempfile::tempdir().unwrap().into_path();
    let prev = std::env::var("RUBBERDUX_HOME").ok();
    // Edition 2024 marks env mutation `unsafe`; `ENV_LOCK` keeps it serialized.
    unsafe {
        std::env::set_var("RUBBERDUX_HOME", &home);
    }

    let client = Arc::new(MoonshotClient::from_env());
    let addr = serve(home.join("apps"), client).await;

    // Subscribe to the board stream before creating so no `updated` is missed.
    let url = format!("ws://{addr}/api/v1/ws/board");
    let (mut socket, _) = connect_async(url).await.unwrap();

    let task = "Plan the Tokyo cherry-blossom trip itinerary for next April";
    let (status, created) = http(
        addr,
        "POST",
        "/api/v1/apps",
        Some(serde_json::json!({
            "task": task,
            "position": { "row": 0, "column": 0 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create must return 201 (body: {created})");
    let id = created["id"].as_str().unwrap().to_owned();
    // The create response carries the placeholder identity — derivation runs
    // after the response.
    assert_eq!(created["icon"]["symbol"], PLACEHOLDER_SYMBOL);

    // First the App appears (`app_created`), then the derivation persists and
    // broadcasts an `updated` event referencing the same id.
    let updated = recv_until(&mut socket, |v| v["type"] == "updated" && v["id"] == id).await;
    assert!(
        updated.is_some(),
        "board stream must emit an `updated` event for the derived identity"
    );

    // `GET /apps` now serves the persisted, derived identity rather than the
    // placeholder seed. The derived identity differs from the placeholder in at
    // least one field (title is no longer the raw task, or the icon changed).
    let got = http(addr, "GET", &format!("/api/v1/apps/{id}"), None).await.1;
    let derived_title = got["title"].as_str().unwrap();
    let derived_symbol = got["icon"]["symbol"].as_str().unwrap();
    let derived_color = got["icon"]["color"].as_str().unwrap();
    let differs = derived_title != task
        || derived_symbol != PLACEHOLDER_SYMBOL
        || derived_color != PLACEHOLDER_COLOR;
    assert!(
        differs,
        "persisted identity must differ from the placeholder: title={derived_title:?} symbol={derived_symbol:?} color={derived_color:?}"
    );

    // The on-disk manifest matches what `GET /apps` serves — the persistence is
    // genuine, not a runtime overlay.
    let store = FilesystemAppStore::with_apps_dir(home.join("apps"));
    let on_disk = store.get(&AppId(id.clone())).unwrap().unwrap();
    assert_eq!(on_disk.title, derived_title);
    assert_eq!(on_disk.icon.symbol, derived_symbol);

    unsafe {
        match prev {
            Some(v) => std::env::set_var("RUBBERDUX_HOME", v),
            None => std::env::remove_var("RUBBERDUX_HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&home);
}
