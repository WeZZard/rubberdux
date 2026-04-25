use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Messages sent from a VM agent to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentToHost {
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
    },
}

/// Messages sent from the host to a VM agent.
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
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_roundtrip_agent_to_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let msg = AgentToHost::SpawnVM {
            task_id: "t1".into(),
            prompt: "do stuff".into(),
            subagent_type: "computer_use".into(),
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
                },
                AgentToHost::SpawnVM {
                    task_id: t2,
                    prompt: p2,
                    subagent_type: s2,
                },
            ) => {
                assert_eq!(t1, t2);
                assert_eq!(p1, p2);
                assert_eq!(s1, s2);
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
