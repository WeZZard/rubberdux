use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Messages sent from a worker (VM agent or native app worker) to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentToHost {
    /// The first frame a native app worker sends after connecting, identifying
    /// which App this socket belongs to so the host can route it without relying
    /// on accept order. See `docs/app/runtime/worker-lifecycle.md`.
    Hello { app_id: String },
    /// A history entry produced by a native app worker's `AgentLoop`, streamed to
    /// the host as it is appended. Mirrors
    /// [`crate::agent::runtime::port::EntryNotification`].
    EntryNotification {
        entry: crate::agent::entry::Entry,
        is_final: bool,
    },
    /// A native app worker addresses another peer App. The host's peer broker
    /// (defined later) routes the payload; here the frame is only declared.
    /// See `docs/app/runtime/worker-lifecycle.md`.
    PeerSend {
        to: String,
        payload: serde_json::Value,
    },
    /// A native app worker requests the set of peers it may address. The host
    /// answers with [`HostToAgent::PeerListResult`].
    PeerList,
    /// Agent response to a user message.
    Response {
        text: String,
        entry_id: usize,
        is_final: bool,
        reply_to_message_id: Option<i32>,
    },
    /// Request the host to spawn a child VM for a general-purpose subagent.
    SpawnVM {
        task_id: String,
        prompt: String,
        subagent_type: String,
        agent_name: Option<String>,
    },
    /// VM child's external agent needs user input.
    ExternalInteraction {
        task_id: String,
        request: crate::agent::external::UIInteractionRequest,
    },
    /// A native app worker raised a unified-vocabulary interaction for the user
    /// to answer. The host replies with [`HostToAgent::InteractionResponse`].
    /// See `docs/agent/interaction.md`.
    Interaction {
        interaction: crate::agent::interaction::AgentInteraction,
    },
}

/// Messages sent from the host to a worker (VM agent or native app worker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostToAgent {
    /// Forwarded user message from Telegram.
    UserMessage {
        text: String,
        telegram_message_id: Option<i32>,
    },
    /// A child VM completed its task.
    VMCompleted { task_id: String, result: String },
    /// A child VM failed.
    VMFailed { task_id: String, error: String },
    /// Shutdown signal.
    Shutdown,
    /// Response to an external agent interaction (from user or LLM).
    InteractionResponse {
        request_id: String,
        response: crate::agent::external::UIInteractionResponse,
    },
    /// A peer App delivered a message to this native app worker. Counterpart of
    /// [`AgentToHost::PeerSend`]; the broker that produces it is defined later.
    /// See `docs/app/runtime/worker-lifecycle.md`.
    PeerDeliver {
        from: String,
        payload: serde_json::Value,
    },
    /// The host's answer to [`AgentToHost::PeerList`]: the App ids this worker may
    /// address.
    PeerListResult { peers: Vec<String> },
    /// The host's reply to an [`AgentToHost::Interaction`], using the unified
    /// interaction vocabulary. See `docs/agent/interaction.md`.
    InteractionAnswer {
        response: crate::agent::interaction::InteractionResponse,
    },
}

// ---------------------------------------------------------------------------
// Framing: length-prefixed JSON over TCP
// ---------------------------------------------------------------------------

/// Write a JSON message preceded by a 4-byte big-endian length.
pub async fn write_message<T: Serialize>(
    writer: &mut OwnedWriteHalf,
    msg: &T,
) -> Result<(), crate::error::Error> {
    let payload = serde_json::to_vec(msg)?;
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON message. Returns `None` on clean EOF.
pub async fn read_message<T: for<'de> Deserialize<'de>>(
    reader: &mut OwnedReadHalf,
) -> Result<Option<T>, crate::error::Error> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(crate::error::Error::Rpc(format!(
            "message too large: {} bytes",
            len
        )));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let msg = serde_json::from_slice(&buf)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::external::{UIInteractionRequest, UIInteractionResponse, QuestionOption};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_roundtrip_agent_to_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let msg = AgentToHost::SpawnVM {
            task_id: "t1".into(),
            prompt: "do stuff".into(),
            subagent_type: "computer_use".into(),
            agent_name: None,
        };

        let send_msg = msg.clone();
        let sender = tokio::spawn(async move {
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (_r, mut w) = stream.into_split();
            write_message(&mut w, &send_msg).await.unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let (mut r, _w) = stream.into_split();
        let received: AgentToHost = read_message(&mut r).await.unwrap().unwrap();

        sender.await.unwrap();

        match (&msg, &received) {
            (
                AgentToHost::SpawnVM {
                    task_id: t1,
                    prompt: p1,
                    subagent_type: s1,
                    agent_name: a1,
                },
                AgentToHost::SpawnVM {
                    task_id: t2,
                    prompt: p2,
                    subagent_type: s2,
                    agent_name: a2,
                },
            ) => {
                assert_eq!(t1, t2);
                assert_eq!(p1, p2);
                assert_eq!(s1, s2);
                assert_eq!(a1, a2);
            }
            _ => panic!("message mismatch"),
        }
    }

    #[tokio::test]
    async fn test_roundtrip_host_to_agent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let msg = HostToAgent::UserMessage {
            text: "hello".into(),
            telegram_message_id: Some(42),
        };

        let send_msg = msg.clone();
        let sender = tokio::spawn(async move {
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (_r, mut w) = stream.into_split();
            write_message(&mut w, &send_msg).await.unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let (mut r, _w) = stream.into_split();
        let received: HostToAgent = read_message(&mut r).await.unwrap().unwrap();

        sender.await.unwrap();

        match (&msg, &received) {
            (
                HostToAgent::UserMessage {
                    text: t1,
                    telegram_message_id: id1,
                },
                HostToAgent::UserMessage {
                    text: t2,
                    telegram_message_id: id2,
                },
            ) => {
                assert_eq!(t1, t2);
                assert_eq!(id1, id2);
            }
            _ => panic!("message mismatch"),
        }
    }

    #[test]
    fn test_external_interaction_roundtrip() {
        let msg = AgentToHost::ExternalInteraction {
            task_id: "task-1".into(),
            request: UIInteractionRequest::Question {
                request_id: "q-1".into(),
                agent_task_id: "task-1".into(),
                text: "Which database?".into(),
                options: vec![
                    QuestionOption { label: "PostgreSQL".into(), description: "relational".into() },
                ],
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentToHost = serde_json::from_str(&json).unwrap();
        match back {
            AgentToHost::ExternalInteraction { task_id, request } => {
                assert_eq!(task_id, "task-1");
                match request {
                    UIInteractionRequest::Question { request_id, text, options, .. } => {
                        assert_eq!(request_id, "q-1");
                        assert_eq!(text, "Which database?");
                        assert_eq!(options.len(), 1);
                    }
                    _ => panic!("Expected Question"),
                }
            }
            _ => panic!("Expected ExternalInteraction"),
        }
    }

    #[test]
    fn test_interaction_response_roundtrip() {
        let msg = HostToAgent::InteractionResponse {
            request_id: "q-1".into(),
            response: UIInteractionResponse::SelectedOption {
                request_id: "q-1".into(),
                index: 2,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: HostToAgent = serde_json::from_str(&json).unwrap();
        match back {
            HostToAgent::InteractionResponse { request_id, response } => {
                assert_eq!(request_id, "q-1");
                match response {
                    UIInteractionResponse::SelectedOption { index, .. } => {
                        assert_eq!(index, 2);
                    }
                    _ => panic!("Expected SelectedOption"),
                }
            }
            _ => panic!("Expected InteractionResponse"),
        }
    }

    #[test]
    fn test_external_interaction_json_format() {
        let msg = AgentToHost::ExternalInteraction {
            task_id: "t-1".into(),
            request: UIInteractionRequest::PermissionRequest {
                request_id: "perm-1".into(),
                agent_task_id: "t-1".into(),
                description: "run cargo test".into(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("ExternalInteraction"));
        assert!(json.contains("task_id"));
        assert!(json.contains("perm-1"));
    }

    #[test]
    fn test_interaction_response_json_format() {
        let msg = HostToAgent::InteractionResponse {
            request_id: "p-1".into(),
            response: UIInteractionResponse::PlanApproved {
                request_id: "p-1".into(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("InteractionResponse"));
        assert!(json.contains("request_id"));
        assert!(json.contains("PlanApproved"));
    }

    #[test]
    fn test_spawn_vm_with_agent_name_roundtrip() {
        let msg = AgentToHost::SpawnVM {
            task_id: "t-1".into(),
            prompt: "do stuff".into(),
            subagent_type: "external".into(),
            agent_name: Some("claude_code".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentToHost = serde_json::from_str(&json).unwrap();
        match back {
            AgentToHost::SpawnVM { task_id, agent_name, .. } => {
                assert_eq!(task_id, "t-1");
                assert_eq!(agent_name, Some("claude_code".into()));
            }
            _ => panic!("Expected SpawnVM"),
        }
    }

    #[test]
    fn test_spawn_vm_without_agent_name_roundtrip() {
        let msg = AgentToHost::SpawnVM {
            task_id: "t-2".into(),
            prompt: "do stuff".into(),
            subagent_type: "computer_use".into(),
            agent_name: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentToHost = serde_json::from_str(&json).unwrap();
        match back {
            AgentToHost::SpawnVM { agent_name, .. } => {
                assert_eq!(agent_name, None);
            }
            _ => panic!("Expected SpawnVM"),
        }
    }

    #[test]
    fn test_hello_roundtrip() {
        let msg = AgentToHost::Hello {
            app_id: "2026-06-10-00-00-00-UTC".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Hello"));
        let back: AgentToHost = serde_json::from_str(&json).unwrap();
        match back {
            AgentToHost::Hello { app_id } => assert_eq!(app_id, "2026-06-10-00-00-00-UTC"),
            _ => panic!("Expected Hello"),
        }
    }

    #[test]
    fn test_entry_notification_roundtrip() {
        use crate::agent::entry::{Entry, EntryOrigin};
        use crate::provider::moonshot::UserContent;

        let msg = AgentToHost::EntryNotification {
            entry: Entry {
                id: 7,
                parent_id: Some(3),
                message: crate::provider::moonshot::Message::User {
                    content: UserContent::Text("hi".into()),
                },
                origin: EntryOrigin::User { channel: "board".into() },
                channel_metadata: None,
            },
            is_final: true,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentToHost = serde_json::from_str(&json).unwrap();
        match back {
            AgentToHost::EntryNotification { entry, is_final } => {
                assert_eq!(entry.id, 7);
                assert_eq!(entry.message.content_text(), "hi");
                assert!(is_final);
            }
            _ => panic!("Expected EntryNotification"),
        }
    }

    #[test]
    fn test_peer_send_and_list_roundtrip() {
        let send = AgentToHost::PeerSend {
            to: "app-2".into(),
            payload: serde_json::json!({"text": "ping"}),
        };
        let back: AgentToHost = serde_json::from_str(&serde_json::to_string(&send).unwrap()).unwrap();
        match back {
            AgentToHost::PeerSend { to, payload } => {
                assert_eq!(to, "app-2");
                assert_eq!(payload["text"], "ping");
            }
            _ => panic!("Expected PeerSend"),
        }

        let list = AgentToHost::PeerList;
        let back: AgentToHost = serde_json::from_str(&serde_json::to_string(&list).unwrap()).unwrap();
        assert!(matches!(back, AgentToHost::PeerList));
    }

    #[test]
    fn test_peer_deliver_and_list_result_roundtrip() {
        let deliver = HostToAgent::PeerDeliver {
            from: "app-1".into(),
            payload: serde_json::json!({"text": "pong"}),
        };
        let back: HostToAgent =
            serde_json::from_str(&serde_json::to_string(&deliver).unwrap()).unwrap();
        match back {
            HostToAgent::PeerDeliver { from, payload } => {
                assert_eq!(from, "app-1");
                assert_eq!(payload["text"], "pong");
            }
            _ => panic!("Expected PeerDeliver"),
        }

        let result = HostToAgent::PeerListResult {
            peers: vec!["app-1".into(), "app-2".into()],
        };
        let back: HostToAgent =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
        match back {
            HostToAgent::PeerListResult { peers } => assert_eq!(peers.len(), 2),
            _ => panic!("Expected PeerListResult"),
        }
    }

    #[test]
    fn test_interaction_frames_roundtrip() {
        use crate::agent::interaction::{
            AgentInteraction, ApprovalFlavor, InteractionResponse,
        };

        let msg = AgentToHost::Interaction {
            interaction: AgentInteraction::Approval {
                request_id: "r1".into(),
                app_id: "app-1".into(),
                flavor: ApprovalFlavor::Permission,
                prompt: "delete?".into(),
            },
        };
        let back: AgentToHost = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        match back {
            AgentToHost::Interaction { interaction } => {
                assert_eq!(interaction.request_id(), "r1");
            }
            _ => panic!("Expected Interaction"),
        }

        let answer = HostToAgent::InteractionAnswer {
            response: InteractionResponse::Approved {
                request_id: "r1".into(),
                flavor: ApprovalFlavor::Permission,
            },
        };
        let back: HostToAgent =
            serde_json::from_str(&serde_json::to_string(&answer).unwrap()).unwrap();
        match back {
            HostToAgent::InteractionAnswer { response } => {
                assert_eq!(response.request_id(), "r1");
            }
            _ => panic!("Expected InteractionAnswer"),
        }
    }

    #[tokio::test]
    async fn test_eof_returns_none() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let sender = tokio::spawn(async move {
            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            drop(stream); // immediate close
        });

        let (stream, _) = listener.accept().await.unwrap();
        let (mut r, _w) = stream.into_split();
        let result: Option<HostToAgent> = read_message(&mut r).await.unwrap();
        assert!(result.is_none());

        sender.await.unwrap();
    }
}
