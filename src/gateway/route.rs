use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::agent::entry::Entry;
use crate::provider::moonshot::Message;
use crate::provider::moonshot::tool::ToolCall;

use super::error::GatewayError;
use super::state::GatewayState;

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct EntriesResponse {
    entries: Vec<Entry>,
}

#[derive(Serialize)]
struct PromptResponse {
    content: String,
}

#[derive(Serialize)]
struct ToolCallPair {
    entry_id: usize,
    call: ToolCall,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ToolResultInfo>,
}

#[derive(Serialize)]
struct ToolResultInfo {
    entry_id: usize,
    tool_call_id: String,
    content: String,
}

#[derive(Serialize)]
struct ToolCallsResponse {
    tool_calls: Vec<ToolCallPair>,
}

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ListEntriesParams {
    pub role: Option<String>,
    pub since_id: Option<usize>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> axum::Router<Arc<GatewayState>> {
    axum::Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/entries", get(list_entries))
        .route("/api/v1/entries/{id}", get(get_entry))
        .route("/api/v1/tool-calls", get(list_tool_calls))
        .route("/api/v1/prompts/system", get(get_system_prompt))
        .route("/api/v1/prompts/identity", get(get_identity_prompt))
        .route("/api/v1/prompts/soul", get(get_soul_prompt))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn list_entries(
    State(state): State<Arc<GatewayState>>,
    Query(params): Query<ListEntriesParams>,
) -> Result<Json<EntriesResponse>, GatewayError> {
    let entries = state.entries.read().await;

    let result: Vec<Entry> = entries
        .iter()
        .filter(|e| match params.role {
            Some(ref role) => message_role(&e.message) == role.as_str(),
            None => true,
        })
        .filter(|e| match params.since_id {
            Some(since) => e.id > since,
            None => true,
        })
        .cloned()
        .collect();

    Ok(Json(EntriesResponse { entries: result }))
}

async fn get_entry(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<usize>,
) -> Result<Json<Entry>, GatewayError> {
    let entries = state.entries.read().await;
    let entry = entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .ok_or(GatewayError::EntryNotFound(id))?;
    Ok(Json(entry))
}

async fn list_tool_calls(
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<ToolCallsResponse>, GatewayError> {
    let entries = state.entries.read().await;

    let mut pairs: Vec<ToolCallPair> = Vec::new();

    for entry in entries.iter() {
        if let Message::Assistant { tool_calls: Some(calls), .. } = &entry.message {
            for call in calls {
                let result = entries.iter().find_map(|e| {
                    if let Message::Tool {
                        tool_call_id,
                        content,
                        ..
                    } = &e.message
                    {
                        if tool_call_id == &call.id {
                            Some(ToolResultInfo {
                                entry_id: e.id,
                                tool_call_id: tool_call_id.clone(),
                                content: content.clone(),
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                pairs.push(ToolCallPair {
                    entry_id: entry.id,
                    call: call.clone(),
                    result,
                });
            }
        }
    }

    Ok(Json(ToolCallsResponse { tool_calls: pairs }))
}

async fn get_system_prompt(
    State(state): State<Arc<GatewayState>>,
) -> Json<PromptResponse> {
    Json(PromptResponse {
        content: state.system_prompt.clone(),
    })
}

async fn get_identity_prompt(
    State(state): State<Arc<GatewayState>>,
) -> Json<PromptResponse> {
    Json(PromptResponse {
        content: state.identity_prompt.clone(),
    })
}

async fn get_soul_prompt(
    State(state): State<Arc<GatewayState>>,
) -> Json<PromptResponse> {
    Json(PromptResponse {
        content: state.soul_prompt.clone(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the serde tag value for a Message variant.
fn message_role(msg: &Message) -> &'static str {
    match msg {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
        Message::Tool { .. } => "tool",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::provider::moonshot::UserContent;
    use crate::provider::moonshot::tool::FunctionCall;

    async fn test_state() -> Arc<GatewayState> {
        let state = GatewayState::new(
            "You are helpful.".into(),
            "You are an AI assistant.".into(),
            "Be kind and thoughtful.".into(),
        );

        let entries = vec![
            Entry {
                id: 0,
                parent_id: None,
                message: Message::System {
                    content: "You are helpful.".into(),
                },
            },
            Entry {
                id: 1,
                parent_id: None,
                message: Message::User {
                    content: UserContent::Text("Hello".into()),
                },
            },
            Entry {
                id: 2,
                parent_id: Some(1),
                message: Message::Assistant {
                    content: Some("Let me look that up.".into()),
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        index: Some(0),
                        id: "call_abc".into(),
                        r#type: "function".into(),
                        function: FunctionCall {
                            name: "web_fetch".into(),
                            arguments: r#"{"url":"https://example.com"}"#.into(),
                        },
                        depends_on: None,
                    }]),
                    partial: None,
                },
            },
            Entry {
                id: 3,
                parent_id: Some(2),
                message: Message::Tool {
                    tool_call_id: "call_abc".into(),
                    name: None,
                    content: "Example Domain page content".into(),
                },
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

    async fn app() -> axum::Router {
        router().with_state(test_state().await)
    }

    fn empty_app() -> axum::Router {
        let state = Arc::new(GatewayState::new(
            "sys".into(),
            "id".into(),
            "soul".into(),
        ));
        router().with_state(state)
    }

    #[tokio::test]
    async fn test_health_returns_ok() {
        let (status, json) = response_json(app().await, "/api/v1/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn test_list_entries_empty() {
        let (status, json) = response_json(empty_app(), "/api/v1/entries").await;
        assert_eq!(status, StatusCode::OK);
        let entries = json["entries"].as_array().unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_list_entries_with_data() {
        let (status, json) = response_json(app().await, "/api/v1/entries").await;
        assert_eq!(status, StatusCode::OK);
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["id"], 0);
        assert_eq!(entries[3]["id"], 3);
    }

    #[tokio::test]
    async fn test_list_entries_filter_role() {
        let (status, json) =
            response_json(app().await, "/api/v1/entries?role=user").await;
        assert_eq!(status, StatusCode::OK);
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["message"]["role"], "user");

        // Filter by assistant
        let (_, json) =
            response_json(app().await, "/api/v1/entries?role=assistant").await;
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], 2);

        // Filter by tool
        let (_, json) =
            response_json(app().await, "/api/v1/entries?role=tool").await;
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], 3);

        // since_id filter
        let (_, json) =
            response_json(app().await, "/api/v1/entries?since_id=1").await;
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["id"], 2);
        assert_eq!(entries[1]["id"], 3);
    }

    #[tokio::test]
    async fn test_get_entry_found() {
        let (status, json) = response_json(app().await, "/api/v1/entries/1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["id"], 1);
        assert_eq!(json["message"]["role"], "user");
    }

    #[tokio::test]
    async fn test_get_entry_not_found() {
        let req = Request::builder()
            .uri("/api/v1/entries/999")
            .body(Body::empty())
            .unwrap();
        let resp = app().await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_system_prompt() {
        let (status, json) =
            response_json(app().await, "/api/v1/prompts/system").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["content"], "You are helpful.");
    }

    #[tokio::test]
    async fn test_get_identity_prompt() {
        let (status, json) =
            response_json(app().await, "/api/v1/prompts/identity").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["content"], "You are an AI assistant.");
    }

    #[tokio::test]
    async fn test_get_soul_prompt() {
        let (status, json) =
            response_json(app().await, "/api/v1/prompts/soul").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["content"], "Be kind and thoughtful.");
    }

    #[tokio::test]
    async fn test_list_tool_calls() {
        let (status, json) =
            response_json(app().await, "/api/v1/tool-calls").await;
        assert_eq!(status, StatusCode::OK);
        let tool_calls = json["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);

        let pair = &tool_calls[0];
        assert_eq!(pair["entry_id"], 2);
        assert_eq!(pair["call"]["id"], "call_abc");
        assert_eq!(pair["call"]["function"]["name"], "web_fetch");

        let result = &pair["result"];
        assert_eq!(result["entry_id"], 3);
        assert_eq!(result["tool_call_id"], "call_abc");
        assert_eq!(result["content"], "Example Domain page content");
    }
}
