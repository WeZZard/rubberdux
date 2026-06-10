//! The agent-facing `peer_list` / `peer_send` tools.
//!
//! These give an App's agent the two verbs of the peer-messaging network:
//!
//! - `peer_list` — ask "who can I talk to right now?". Returns the App ids of the
//!   peers currently reachable, most-recently-used first.
//! - `peer_send` — send an opaque message to a peer by its App id. The host's
//!   broker (`crate::host::PeerBroker`) relays it: delivered if the target is
//!   live, otherwise queued to the target's inbox and the target is woken.
//!
//! The tools do not talk to the broker directly. A native App worker
//! (`crate::app::runtime::worker`) services them over a [`PeerChannel`] seam: the
//! worker writes the `AgentToHost::PeerSend` / `AgentToHost::PeerList` frame and,
//! for a list, returns the host's `PeerListResult`. Keeping the broker behind the
//! worker's RPC duplex means the tool stays the same whether the broker is local
//! or, later, federated across machines. These tools are registered **only in App
//! workers** (see `crate::agent::builder`), since only a worker has a peer
//! transport. The design is in `docs/app/peer/decentralized-messaging.md`.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::{mpsc, oneshot};

use crate::provider::moonshot::tool::ToolDefinition;

use super::{Tool, ToolOutcome};

/// One request a peer tool asks its worker to perform over the RPC duplex. The
/// worker translates each into the matching `AgentToHost` frame; a `List` also
/// carries a oneshot for the worker to return the host's `PeerListResult`.
pub enum PeerRequest {
    /// Relay an opaque payload to a peer App by id. Fire-and-forget: the broker
    /// either delivers it or queues it, and either is success from the sender's
    /// view, so no reply channel is needed.
    Send {
        to: String,
        payload: serde_json::Value,
    },
    /// Ask the host for the reachable peers; the worker answers via `reply`.
    List {
        reply: oneshot::Sender<Vec<String>>,
    },
}

/// The seam the peer tools use to reach their worker's RPC duplex. Cloneable so
/// both `peer_list` and `peer_send` share one channel into the worker's bridge
/// loop, which performs the actual frame I/O.
#[derive(Clone)]
pub struct PeerChannel {
    tx: mpsc::Sender<PeerRequest>,
}

impl PeerChannel {
    /// Wrap the sending half of the worker's peer-request channel.
    pub fn new(tx: mpsc::Sender<PeerRequest>) -> Self {
        Self { tx }
    }

    /// Queue a `peer_send` for the worker to forward to the host.
    pub async fn send(&self, to: String, payload: serde_json::Value) -> Result<(), crate::error::Error> {
        self.tx
            .send(PeerRequest::Send { to, payload })
            .await
            .map_err(|_| crate::error::Error::Peer("worker peer channel is closed".into()))
    }

    /// Ask the worker for the reachable peers and await the host's answer.
    pub async fn list(&self) -> Result<Vec<String>, crate::error::Error> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(PeerRequest::List { reply })
            .await
            .map_err(|_| crate::error::Error::Peer("worker peer channel is closed".into()))?;
        rx.await
            .map_err(|_| crate::error::Error::Peer("worker did not answer peer_list".into()))
    }
}

/// The `peer_list` tool: returns the reachable peers, most-recently-used first.
pub struct PeerListTool {
    channel: PeerChannel,
}

impl PeerListTool {
    pub fn new(channel: PeerChannel) -> Self {
        Self { channel }
    }
}

impl Tool for PeerListTool {
    fn name(&self) -> &str {
        "peer_list"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "peer_list",
            "List the peer Apps you can message right now, most-recently-used \
             first. Returns their App ids. Use this to discover who is available \
             before peer_send.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            match self.channel.list().await {
                Ok(peers) => {
                    let content = if peers.is_empty() {
                        "No peers are reachable right now.".to_string()
                    } else {
                        let body = peers
                            .iter()
                            .map(|id| format!("- {id}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("Reachable peers (most recent first):\n{body}")
                    };
                    ToolOutcome::Immediate { content, is_error: false }
                }
                Err(e) => ToolOutcome::Immediate {
                    content: format!("Failed to list peers: {e}"),
                    is_error: true,
                },
            }
        })
    }
}

/// The `peer_send` tool: send an opaque message to a peer App by id.
pub struct PeerSendTool {
    channel: PeerChannel,
}

impl PeerSendTool {
    pub fn new(channel: PeerChannel) -> Self {
        Self { channel }
    }
}

impl Tool for PeerSendTool {
    fn name(&self) -> &str {
        "peer_send"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "peer_send",
            "Send a message to another App by its App id (from peer_list). The \
             message reaches the peer whether it is currently active or offline; \
             an offline peer receives it when it next wakes. Provide `to` (the \
             target App id) and `message` (the text to send).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "The target App id, as returned by peer_list."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message text to deliver to the peer."
                    }
                },
                "required": ["to", "message"],
                "additionalProperties": false
            }),
        )
    }

    fn execute<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(async move {
            let args: serde_json::Value = match serde_json::from_str(arguments) {
                Ok(v) => v,
                Err(e) => {
                    return ToolOutcome::Immediate {
                        content: format!("Failed to parse tool arguments: {e}"),
                        is_error: true,
                    };
                }
            };
            let to = match args["to"].as_str() {
                Some(to) if !to.is_empty() => to.to_string(),
                _ => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: to".into(),
                        is_error: true,
                    };
                }
            };
            let message = match args["message"].as_str() {
                Some(m) => m.to_string(),
                None => {
                    return ToolOutcome::Immediate {
                        content: "Missing required parameter: message".into(),
                        is_error: true,
                    };
                }
            };
            // The payload is opaque to the broker; carry the text under a `text`
            // key so the delivered envelope is self-describing.
            let payload = serde_json::json!({ "text": message });
            match self.channel.send(to.clone(), payload).await {
                Ok(()) => ToolOutcome::Immediate {
                    content: format!("Message sent to peer `{to}`."),
                    is_error: false,
                },
                Err(e) => ToolOutcome::Immediate {
                    content: format!("Failed to send to peer `{to}`: {e}"),
                    is_error: true,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_tool_names_are_domain_facing() {
        let (tx, _rx) = mpsc::channel(1);
        let channel = PeerChannel::new(tx);
        assert_eq!(PeerListTool::new(channel.clone()).name(), "peer_list");
        assert_eq!(PeerSendTool::new(channel).name(), "peer_send");
    }

    #[test]
    fn peer_send_definition_requires_to_and_message() {
        let (tx, _rx) = mpsc::channel(1);
        let def = PeerSendTool::new(PeerChannel::new(tx)).definition();
        let params = def.function.parameters.unwrap();
        assert_eq!(params["required"], serde_json::json!(["to", "message"]));
    }

    #[tokio::test]
    async fn peer_send_forwards_a_request_to_the_worker() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PeerSendTool::new(PeerChannel::new(tx));
        let outcome = tool
            .execute(r#"{"to": "app-2", "message": "ping"}"#)
            .await;
        match outcome {
            ToolOutcome::Immediate { is_error, content } => {
                assert!(!is_error);
                assert!(content.contains("app-2"));
            }
            _ => panic!("expected Immediate"),
        }
        match rx.recv().await.unwrap() {
            PeerRequest::Send { to, payload } => {
                assert_eq!(to, "app-2");
                assert_eq!(payload["text"], "ping");
            }
            _ => panic!("expected Send"),
        }
    }

    #[tokio::test]
    async fn peer_send_rejects_missing_target() {
        let (tx, _rx) = mpsc::channel(1);
        let tool = PeerSendTool::new(PeerChannel::new(tx));
        let outcome = tool.execute(r#"{"message": "x"}"#).await;
        match outcome {
            ToolOutcome::Immediate { is_error, content } => {
                assert!(is_error);
                assert!(content.contains("to"));
            }
            _ => panic!("expected Immediate"),
        }
    }

    #[tokio::test]
    async fn peer_list_returns_the_workers_answer() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PeerListTool::new(PeerChannel::new(tx));
        // Service the request from a parallel task, as the worker bridge would.
        let servicer = tokio::spawn(async move {
            match rx.recv().await.unwrap() {
                PeerRequest::List { reply } => {
                    reply.send(vec!["app-1".into(), "app-2".into()]).unwrap();
                }
                _ => panic!("expected List"),
            }
        });
        let outcome = tool.execute("{}").await;
        servicer.await.unwrap();
        match outcome {
            ToolOutcome::Immediate { is_error, content } => {
                assert!(!is_error);
                assert!(content.contains("app-1"));
                assert!(content.contains("app-2"));
            }
            _ => panic!("expected Immediate"),
        }
    }

    #[tokio::test]
    async fn peer_list_reports_empty_when_no_peers() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PeerListTool::new(PeerChannel::new(tx));
        let servicer = tokio::spawn(async move {
            match rx.recv().await.unwrap() {
                PeerRequest::List { reply } => reply.send(Vec::new()).unwrap(),
                _ => panic!("expected List"),
            }
        });
        let outcome = tool.execute("{}").await;
        servicer.await.unwrap();
        match outcome {
            ToolOutcome::Immediate { content, .. } => {
                assert!(content.contains("No peers"));
            }
            _ => panic!("expected Immediate"),
        }
    }

    #[tokio::test]
    async fn peer_tool_errors_when_worker_channel_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx); // the worker bridge is gone
        let tool = PeerSendTool::new(PeerChannel::new(tx));
        let outcome = tool.execute(r#"{"to": "x", "message": "y"}"#).await;
        match outcome {
            ToolOutcome::Immediate { is_error, .. } => assert!(is_error),
            _ => panic!("expected Immediate"),
        }
    }
}
