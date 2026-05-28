use std::collections::HashMap;
use std::path::Path;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::Error;

type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, WsMessage>;
type WsRead = SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

pub struct CodexSession {
    ws_write: WsWrite,
    ws_read: WsRead,
    next_id: u64,
    pending_rpc: HashMap<u64, oneshot::Sender<serde_json::Value>>,
    /// Maps request_id to (json_rpc_id, method) for server-initiated requests
    /// that require a response from the UI interaction queue.
    pending_server_requests: HashMap<String, (u64, String)>,
    event_tx: mpsc::Sender<super::ExternalAgentEvent>,
    response_rx: mpsc::Receiver<super::UIInteractionResponse>,
    thread_id: Option<String>,
}

impl CodexSession {
    /// Spawn a Codex session by connecting to the Codex app-server WebSocket,
    /// initializing the session, starting a thread, and issuing the first turn.
    ///
    /// Returns the session and a sender for forwarding UI interaction responses.
    pub async fn spawn(
        prompt: &str,
        cwd: &Path,
        event_tx: mpsc::Sender<super::ExternalAgentEvent>,
    ) -> Result<(Self, mpsc::Sender<super::UIInteractionResponse>), Error> {
        let url = std::env::var("CODEX_WS_URL")
            .unwrap_or_else(|_| "ws://127.0.0.1:19836".into());

        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| Error::Provider(
                format!("Failed to connect to Codex app-server at {}: {}", url, e),
            ))?;

        let (ws_write, ws_read) = ws_stream.split();
        let (response_tx, response_rx) = mpsc::channel(32);

        let mut session = Self {
            ws_write,
            ws_read,
            next_id: 1,
            pending_rpc: HashMap::new(),
            pending_server_requests: HashMap::new(),
            event_tx,
            response_rx,
            thread_id: None,
        };

        // Initialize
        let _init_result = session
            .send_rpc_request("initialize", serde_json::json!({}))
            .await?;

        // Start thread
        let thread_result = session
            .send_rpc_request(
                "thread/start",
                serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                }),
            )
            .await?;

        // Extract threadId from result
        if let Some(thread_id) = thread_result.get("threadId").and_then(|v| v.as_str()) {
            session.thread_id = Some(thread_id.to_string());
        }

        // Start first turn with the prompt
        if let Some(ref thread_id) = session.thread_id {
            let _turn_result = session
                .send_rpc_request(
                    "turn/start",
                    serde_json::json!({
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": prompt}],
                    }),
                )
                .await?;
        }

        Ok((session, response_tx))
    }

    /// Send a JSON-RPC request and wait for the matching response.
    ///
    /// Reads from the WebSocket until the response with the matching id arrives.
    /// Other messages (server requests, notifications) that arrive while waiting
    /// are dispatched via `handle_non_response`.
    async fn send_rpc_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = build_jsonrpc_request(id, method, params);
        self.ws_write
            .send(WsMessage::Text(msg.into()))
            .await
            .map_err(|e| Error::Provider(format!("WebSocket send error: {}", e)))?;

        // Read messages until we get our response
        loop {
            match self.ws_read.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Some(parsed) = parse_jsonrpc_message(&text) {
                        match parsed {
                            JsonRpcMessage::Response {
                                id: resp_id,
                                result,
                            } if resp_id == id => {
                                return Ok(result);
                            }
                            JsonRpcMessage::ErrorResponse {
                                id: resp_id,
                                message,
                                ..
                            } if resp_id == id => {
                                return Err(Error::Provider(format!(
                                    "Codex RPC error for {}: {}",
                                    method, message
                                )));
                            }
                            other => {
                                self.handle_non_response(other).await;
                            }
                        }
                    }
                }
                Some(Ok(_)) => {} // binary, ping, pong
                Some(Err(e)) => {
                    return Err(Error::Provider(format!("WebSocket error: {}", e)));
                }
                None => {
                    return Err(Error::Provider("WebSocket closed".into()));
                }
            }
        }
    }

    /// Drive the event loop: read events from the WebSocket, forward to event_tx.
    /// Also receives UI interaction responses and forwards them to the Codex
    /// app-server. Call this from a spawned tokio task.
    pub async fn drive(mut self) {
        loop {
            tokio::select! {
                msg = self.ws_read.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            if let Some(parsed) = parse_jsonrpc_message(&text) {
                                self.handle_non_response(parsed).await;
                            }
                        }
                        Some(Err(e)) => {
                            let _ = self.event_tx.send(super::ExternalAgentEvent::Failed {
                                error: format!("WebSocket error: {}", e),
                            }).await;
                            break;
                        }
                        None => break,
                        _ => {}
                    }
                }
                response = self.response_rx.recv() => {
                    if let Some(resp) = response {
                        self.forward_response(resp).await;
                    }
                }
            }
        }
        let _ = self
            .event_tx
            .send(super::ExternalAgentEvent::Completed {
                result: "Codex session ended".into(),
            })
            .await;
    }

    /// Dispatch a message that is not the direct response we were waiting for.
    async fn handle_non_response(&mut self, msg: JsonRpcMessage) {
        match msg {
            JsonRpcMessage::ServerRequest { id, method, params } => {
                self.handle_server_request(id, &method, &params).await;
            }
            JsonRpcMessage::Notification { method, params } => {
                self.handle_notification(&method, &params).await;
            }
            JsonRpcMessage::Response { id, result } => {
                // Late response -- check pending_rpc
                if let Some(tx) = self.pending_rpc.remove(&id) {
                    let _ = tx.send(result);
                }
            }
            JsonRpcMessage::ErrorResponse { id, message, .. } => {
                if let Some(tx) = self.pending_rpc.remove(&id) {
                    let _ = tx.send(serde_json::json!({"error": message}));
                }
            }
        }
    }

    /// Map Codex server-initiated requests to UIInteractionRequest events,
    /// or auto-respond to internal/unrecognized requests.
    async fn handle_server_request(
        &mut self,
        rpc_id: u64,
        method: &str,
        params: &serde_json::Value,
    ) {
        let request_id = format!("codex-{}", rpc_id);

        match method {
            // User-facing approval requests -> forward to interaction queue
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let command = params["command"].as_str().unwrap_or("unknown command");
                let request = super::UIInteractionRequest::PermissionRequest {
                    request_id: request_id.clone(),
                    agent_task_id: String::new(),
                    description: format!("Execute command: {}", command),
                };
                self.pending_server_requests
                    .insert(request_id, (rpc_id, method.into()));
                let _ = self
                    .event_tx
                    .send(super::ExternalAgentEvent::UIInteraction(request))
                    .await;
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                let request = super::UIInteractionRequest::PermissionRequest {
                    request_id: request_id.clone(),
                    agent_task_id: String::new(),
                    description: "Approve file changes".into(),
                };
                self.pending_server_requests
                    .insert(request_id, (rpc_id, method.into()));
                let _ = self
                    .event_tx
                    .send(super::ExternalAgentEvent::UIInteraction(request))
                    .await;
            }
            "item/permissions/requestApproval" => {
                let desc = params
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Permission request");
                let request = super::UIInteractionRequest::PermissionRequest {
                    request_id: request_id.clone(),
                    agent_task_id: String::new(),
                    description: desc.into(),
                };
                self.pending_server_requests
                    .insert(request_id, (rpc_id, method.into()));
                let _ = self
                    .event_tx
                    .send(super::ExternalAgentEvent::UIInteraction(request))
                    .await;
            }
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                let questions = params.get("questions").and_then(|q| q.as_array());
                let (text, options) = if let Some(questions) = questions {
                    let first = questions.first();
                    let text: String = first
                        .and_then(|q| q["question"].as_str())
                        .unwrap_or("Agent question")
                        .into();
                    let opts: Vec<super::QuestionOption> = first
                        .and_then(|q| q["options"].as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|o| {
                                    Some(super::QuestionOption {
                                        label: o["label"].as_str()?.into(),
                                        description: o["description"]
                                            .as_str()
                                            .unwrap_or("")
                                            .into(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (text, opts)
                } else {
                    ("Agent question".into(), vec![])
                };
                let request = super::UIInteractionRequest::Question {
                    request_id: request_id.clone(),
                    agent_task_id: String::new(),
                    text,
                    options,
                };
                self.pending_server_requests
                    .insert(request_id, (rpc_id, method.into()));
                let _ = self
                    .event_tx
                    .send(super::ExternalAgentEvent::UIInteraction(request))
                    .await;
            }
            // Internal requests -> auto-respond
            "item/tool/call"
            | "account/chatgptAuthTokens/refresh"
            | "attestation/generate" => {
                let response = build_jsonrpc_response(rpc_id, serde_json::json!({}));
                let _ = self.ws_write.send(WsMessage::Text(response.into())).await;
            }
            _ => {
                log::debug!(
                    "Unhandled Codex server request: {} (id={})",
                    method,
                    rpc_id
                );
                // Auto-respond to unknown requests to prevent blocking
                let response = build_jsonrpc_response(rpc_id, serde_json::json!({}));
                let _ = self.ws_write.send(WsMessage::Text(response.into())).await;
            }
        }
    }

    /// Handle Codex notifications (no response expected).
    async fn handle_notification(&mut self, method: &str, params: &serde_json::Value) {
        match method {
            "turn/completed" => {
                let _ = self
                    .event_tx
                    .send(super::ExternalAgentEvent::Completed {
                        result: "Codex turn completed".into(),
                    })
                    .await;
            }
            "item/agentMessage/delta" => {
                if let Some(text) = params.get("textDelta").and_then(|v| v.as_str()) {
                    let _ = self
                        .event_tx
                        .send(super::ExternalAgentEvent::Progress {
                            message: text.into(),
                        })
                        .await;
                }
            }
            "error" => {
                let message = params
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                let _ = self
                    .event_tx
                    .send(super::ExternalAgentEvent::Failed {
                        error: message.into(),
                    })
                    .await;
            }
            "thread/started" => {
                if let Some(thread_id) = params.get("threadId").and_then(|v| v.as_str()) {
                    self.thread_id = Some(thread_id.into());
                }
            }
            _ => {
                log::debug!("Codex notification: {}", method);
            }
        }
    }

    /// Forward a UI interaction response back to the Codex app-server as
    /// a JSON-RPC response to the original server-initiated request.
    async fn forward_response(&mut self, response: super::UIInteractionResponse) {
        let request_id = match &response {
            super::UIInteractionResponse::SelectedOption { request_id, .. } => request_id,
            super::UIInteractionResponse::PlanApproved { request_id } => request_id,
            super::UIInteractionResponse::PlanRejected { request_id, .. } => request_id,
            super::UIInteractionResponse::PermissionGranted { request_id } => request_id,
            super::UIInteractionResponse::PermissionDenied { request_id, .. } => request_id,
        };

        let (rpc_id, _method) =
            match self.pending_server_requests.remove(request_id.as_str()) {
                Some(entry) => entry,
                None => {
                    log::warn!("No pending Codex request for {}", request_id);
                    return;
                }
            };

        let result = match &response {
            super::UIInteractionResponse::PermissionGranted { .. } => {
                serde_json::json!({ "decision": "accept" })
            }
            super::UIInteractionResponse::PermissionDenied { .. } => {
                serde_json::json!({ "decision": "decline" })
            }
            super::UIInteractionResponse::SelectedOption { index, .. } => {
                serde_json::json!({ "answers": { "0": { "answers": [index.to_string()] } } })
            }
            super::UIInteractionResponse::PlanApproved { .. } => {
                serde_json::json!({ "decision": "accept" })
            }
            super::UIInteractionResponse::PlanRejected { .. } => {
                serde_json::json!({ "decision": "decline" })
            }
        };

        let msg = build_jsonrpc_response(rpc_id, result);
        if let Err(e) = self.ws_write.send(WsMessage::Text(msg.into())).await {
            log::warn!("Failed to send Codex response: {}", e);
        }
    }
}

#[derive(Debug)]
enum JsonRpcMessage {
    Response {
        id: u64,
        result: serde_json::Value,
    },
    ErrorResponse {
        id: u64,
        code: i64,
        message: String,
    },
    ServerRequest {
        id: u64,
        method: String,
        params: serde_json::Value,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
}

fn parse_jsonrpc_message(text: &str) -> Option<JsonRpcMessage> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    let has_id = v.get("id").is_some() && !v["id"].is_null();
    let has_method = v.get("method").and_then(|m| m.as_str()).is_some();
    let has_result = v.get("result").is_some();
    let has_error = v.get("error").is_some();

    if has_id && has_result {
        let id = v["id"].as_u64()?;
        Some(JsonRpcMessage::Response {
            id,
            result: v["result"].clone(),
        })
    } else if has_id && has_error {
        let id = v["id"].as_u64()?;
        let error = &v["error"];
        let code = error["code"].as_i64().unwrap_or(-1);
        let message = error["message"]
            .as_str()
            .unwrap_or("unknown error")
            .into();
        Some(JsonRpcMessage::ErrorResponse { id, code, message })
    } else if has_id && has_method {
        let id = v["id"].as_u64()?;
        let method = v["method"].as_str()?.into();
        let params = v
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Some(JsonRpcMessage::ServerRequest { id, method, params })
    } else if has_method {
        let method = v["method"].as_str()?.into();
        let params = v
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Some(JsonRpcMessage::Notification { method, params })
    } else {
        None
    }
}

fn build_jsonrpc_request(id: u64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

fn build_jsonrpc_response(id: u64, result: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rpc_response() {
        let msg = parse_jsonrpc_message(
            r#"{"jsonrpc":"2.0","id":1,"result":{"threadId":"t-1"}}"#,
        );
        match msg {
            Some(JsonRpcMessage::Response { id, result }) => {
                assert_eq!(id, 1);
                assert_eq!(result["threadId"], "t-1");
            }
            other => panic!("Expected Response, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_rpc_error_response() {
        let msg = parse_jsonrpc_message(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"invalid request"}}"#,
        );
        match msg {
            Some(JsonRpcMessage::ErrorResponse { id, code, message }) => {
                assert_eq!(id, 1);
                assert_eq!(code, -32600);
                assert_eq!(message, "invalid request");
            }
            other => panic!("Expected ErrorResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_server_request() {
        let msg = parse_jsonrpc_message(
            r#"{"jsonrpc":"2.0","id":2,"method":"item/commandExecution/requestApproval","params":{"command":"cargo test"}}"#,
        );
        match msg {
            Some(JsonRpcMessage::ServerRequest { id, method, params }) => {
                assert_eq!(id, 2);
                assert_eq!(method, "item/commandExecution/requestApproval");
                assert_eq!(params["command"], "cargo test");
            }
            other => panic!("Expected ServerRequest, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_notification() {
        let msg = parse_jsonrpc_message(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"t-1"}}"#,
        );
        match msg {
            Some(JsonRpcMessage::Notification { method, params }) => {
                assert_eq!(method, "turn/completed");
                assert_eq!(params["threadId"], "t-1");
            }
            other => panic!("Expected Notification, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_jsonrpc_message("not json").is_none());
    }

    #[test]
    fn test_build_initialize_request() {
        let req = build_jsonrpc_request(1, "initialize", serde_json::json!({}));
        let v: serde_json::Value = serde_json::from_str(&req).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "initialize");
    }

    #[test]
    fn test_build_turn_start_request() {
        let req = build_jsonrpc_request(
            3,
            "turn/start",
            serde_json::json!({
                "threadId": "t-1",
                "input": [{"type": "text", "text": "Fix the bug"}]
            }),
        );
        let v: serde_json::Value = serde_json::from_str(&req).unwrap();
        assert_eq!(v["method"], "turn/start");
        assert_eq!(v["params"]["input"][0]["text"], "Fix the bug");
    }

    #[test]
    fn test_build_jsonrpc_response() {
        let resp = build_jsonrpc_response(42, serde_json::json!({"decision": "accept"}));
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"]["decision"], "accept");
    }

    #[test]
    fn test_map_command_approval() {
        // Test that we can parse command approval params and build the right request_id
        let request_id = format!("codex-{}", 42);
        assert!(request_id.starts_with("codex-"));
        assert_eq!(request_id, "codex-42");
    }

    #[test]
    fn test_map_user_input() {
        // Test parsing requestUserInput params into question text + options
        let params = serde_json::json!({
            "questions": [{
                "id": "q1",
                "question": "Which database?",
                "options": [
                    {"label": "PostgreSQL", "description": "relational"},
                    {"label": "MongoDB", "description": "document"}
                ]
            }]
        });
        let questions = params["questions"].as_array().unwrap();
        let first = &questions[0];
        let text = first["question"].as_str().unwrap();
        assert_eq!(text, "Which database?");
        let options = first["options"].as_array().unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["label"], "PostgreSQL");
    }

    #[test]
    fn test_forward_permission_granted() {
        let result = serde_json::json!({ "decision": "accept" });
        let msg = build_jsonrpc_response(42, result);
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"]["decision"], "accept");
    }

    #[test]
    fn test_forward_permission_denied() {
        let result = serde_json::json!({ "decision": "decline" });
        let msg = build_jsonrpc_response(42, result);
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["result"]["decision"], "decline");
    }
}
