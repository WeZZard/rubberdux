use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use rubberdux::protocol::{self, AgentToHost, HostToAgent};

/// Test that the RPC protocol can roundtrip messages correctly.
#[tokio::test(flavor = "multi_thread")]
async fn test_rpc_roundtrip_agent_to_host() {
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
        protocol::write_message(&mut w, &send_msg).await.unwrap();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut r, _w) = stream.into_split();
    let received: AgentToHost = protocol::read_message(&mut r).await.unwrap().unwrap();

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

/// Test that the RPC protocol can roundtrip host-to-agent messages.
#[tokio::test(flavor = "multi_thread")]
async fn test_rpc_roundtrip_host_to_agent() {
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
        protocol::write_message(&mut w, &send_msg).await.unwrap();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut r, _w) = stream.into_split();
    let received: HostToAgent = protocol::read_message(&mut r).await.unwrap().unwrap();

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

/// Test that clean EOF returns None.
#[tokio::test(flavor = "multi_thread")]
async fn test_rpc_eof_returns_none() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let sender = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        drop(stream); // immediate close
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut r, _w) = stream.into_split();
    let result: Option<HostToAgent> = protocol::read_message(&mut r).await.unwrap();
    assert!(result.is_none());

    sender.await.unwrap();
}

/// Test that oversized messages are rejected.
#[tokio::test(flavor = "multi_thread")]
async fn test_rpc_rejects_oversized_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let sender = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (_r, mut w) = stream.into_split();
        // Write a length prefix for a 20MB message
        let len = (20 * 1024 * 1024u32).to_be_bytes();
        w.write_all(&len).await.unwrap();
        w.flush().await.unwrap();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut r, _w) = stream.into_split();
    let result = protocol::read_message::<HostToAgent>(&mut r).await;
    assert!(result.is_err(), "Expected error for oversized message");

    sender.await.unwrap();
}
