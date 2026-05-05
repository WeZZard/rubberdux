use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use tower::ServiceExt;

use rubberdux::agent::entry::{Entry, EntryOrigin};
use rubberdux::agent::runtime::port::{InputPort, LoopEvent};
use rubberdux::gateway::route::router;
use rubberdux::gateway::state::GatewayState;
use rubberdux::gateway::stream::{ws_entries, ws_trajectory};
use rubberdux::provider::moonshot::{Message, UserContent};
use rubberdux::provider::moonshot::tool::{FunctionCall, ToolCall};

fn dummy_input_port() -> InputPort {
    let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
    InputPort::new(tx)
}

/// Build the full application router (REST + WebSocket routes) with
/// the given state, mirroring what `gateway::server::run` constructs.
fn full_app(state: Arc<GatewayState>) -> axum::Router {
    router()
        .route("/api/v1/ws/entries", get(ws_entries))
        .route("/api/v1/ws/trajectory", get(ws_trajectory))
        .with_state(state)
}

async fn seeded_state() -> Arc<GatewayState> {
    let state = GatewayState::new(
        "You are a test assistant.".into(),
        "Test identity prompt.".into(),
        "Test soul prompt.".into(),
        dummy_input_port(),
    );

    let entries = vec![
        Entry {
            id: 0,
            parent_id: None,
            message: Message::User {
                content: UserContent::Text("What is 2+2?".into()),
            },
            origin: EntryOrigin::User { channel: "test".into() },
            channel_metadata: None,
        },
        Entry {
            id: 1,
            parent_id: Some(0),
            message: Message::Assistant {
                content: Some("The answer is 4.".into()),
                reasoning_content: None,
                tool_calls: None,
                partial: None,
            },
            origin: EntryOrigin::Assistant,
            channel_metadata: None,
        },
        Entry {
            id: 2,
            parent_id: Some(0),
            message: Message::Assistant {
                content: Some("Let me fetch that.".into()),
                reasoning_content: None,
                tool_calls: Some(vec![ToolCall {
                    index: Some(0),
                    id: "call_001".into(),
                    r#type: "function".into(),
                    function: FunctionCall {
                        name: "web_fetch".into(),
                        arguments: r#"{"url":"https://example.com"}"#.into(),
                    },
                    depends_on: None,
                }]),
                partial: None,
            },
            origin: EntryOrigin::Assistant,
            channel_metadata: None,
        },
        Entry {
            id: 3,
            parent_id: Some(2),
            message: Message::Tool {
                tool_call_id: "call_001".into(),
                name: None,
                content: "Example page content".into(),
            },
            origin: EntryOrigin::ToolCall,
            channel_metadata: None,
        },
    ];

    *state.entries.write().await = entries;

    Arc::new(state)
}

async fn response_json(
    app: axum::Router,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

#[tokio::test]
async fn test_full_api_round_trip() {
    let state = seeded_state().await;
    let app = full_app(Arc::clone(&state));

    // --- Health endpoint ---
    let (status, json) = response_json(app, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "ok");

    // --- List all entries ---
    let app = full_app(Arc::clone(&state));
    let (status, json) = response_json(app, "/api/v1/entries").await;
    assert_eq!(status, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0]["id"], 0);
    assert_eq!(entries[3]["id"], 3);

    // --- Get single entry ---
    let app = full_app(Arc::clone(&state));
    let (status, json) = response_json(app, "/api/v1/entries/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["id"], 1);
    assert_eq!(json["message"]["role"], "assistant");

    // --- Entry not found ---
    let app = full_app(Arc::clone(&state));
    let req = Request::builder()
        .uri("/api/v1/entries/999")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // --- Filter entries by role ---
    let app = full_app(Arc::clone(&state));
    let (status, json) =
        response_json(app, "/api/v1/entries?role=user").await;
    assert_eq!(status, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["message"]["role"], "user");

    // --- Filter entries by since_id ---
    let app = full_app(Arc::clone(&state));
    let (status, json) =
        response_json(app, "/api/v1/entries?since_id=1").await;
    assert_eq!(status, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["id"], 2);
    assert_eq!(entries[1]["id"], 3);

    // --- System prompt ---
    let app = full_app(Arc::clone(&state));
    let (status, json) =
        response_json(app, "/api/v1/prompts/system").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["content"], "You are a test assistant.");

    // --- Identity prompt ---
    let app = full_app(Arc::clone(&state));
    let (status, json) =
        response_json(app, "/api/v1/prompts/identity").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["content"], "Test identity prompt.");

    // --- Soul prompt ---
    let app = full_app(Arc::clone(&state));
    let (status, json) =
        response_json(app, "/api/v1/prompts/soul").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["content"], "Test soul prompt.");

    // --- Tool calls endpoint ---
    let app = full_app(Arc::clone(&state));
    let (status, json) =
        response_json(app, "/api/v1/tool-calls").await;
    assert_eq!(status, StatusCode::OK);
    let tool_calls = json["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["entry_id"], 2);
    assert_eq!(tool_calls[0]["call"]["id"], "call_001");
    assert_eq!(tool_calls[0]["call"]["function"]["name"], "web_fetch");
    let result = &tool_calls[0]["result"];
    assert_eq!(result["entry_id"], 3);
    assert_eq!(result["tool_call_id"], "call_001");
    assert_eq!(result["content"], "Example page content");
}

// NOTE: A WebSocket streaming test (test_websocket_entries_stream) is not
// included because `tokio-tungstenite` is only a transitive dependency (via
// axum) and is not exposed as a direct dev-dependency. Adding a WebSocket
// client test requires either:
//   1. Adding `tokio-tungstenite` as an explicit dev-dependency, or
//   2. Using axum's internal test utilities (which are not publicly exposed
//      for WebSocket upgrade testing).
//
// The WebSocket handler logic (`ws_entries`, `ws_trajectory`) is wired into
// the router above and is exercised indirectly by the round-trip test
// confirming the full app builds without route conflicts.
