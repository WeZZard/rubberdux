use rubberdux::agent::external::interaction_queue::{InteractionQueue, PendingInteraction};
use rubberdux::agent::external::{QuestionOption, UIInteractionRequest, UIInteractionResponse};
use rubberdux::protocol::{read_message, write_message, AgentToHost, HostToAgent};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_host_child_external_interaction_roundtrip() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let child = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut r, mut w) = stream.into_split();

        let request = UIInteractionRequest::Question {
            request_id: "q-1".into(),
            agent_task_id: "task-1".into(),
            text: "Which database?".into(),
            options: vec![
                QuestionOption {
                    label: "PostgreSQL".into(),
                    description: "relational".into(),
                },
                QuestionOption {
                    label: "SQLite".into(),
                    description: "embedded".into(),
                },
            ],
        };

        let msg = AgentToHost::ExternalInteraction {
            task_id: "task-1".into(),
            request,
        };
        write_message(&mut w, &msg).await.unwrap();

        let response: HostToAgent = read_message(&mut r).await.unwrap().unwrap();
        response
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut r, mut w) = stream.into_split();

    let received: AgentToHost = read_message(&mut r).await.unwrap().unwrap();

    let queue = InteractionQueue::new();
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

    match &received {
        AgentToHost::ExternalInteraction { request, .. } => {
            let request_id = match request {
                UIInteractionRequest::Question { request_id, .. } => request_id.clone(),
                _ => panic!("expected Question"),
            };
            queue.add(
                request_id,
                PendingInteraction {
                    request: request.clone(),
                    response_tx: resp_tx,
                },
            );
        }
        _ => panic!("expected ExternalInteraction"),
    }

    let resolution = UIInteractionResponse::SelectedOption {
        request_id: "q-1".into(),
        index: 1,
    };
    assert!(queue.resolve("q-1", resolution));

    let resolved = resp_rx.await.unwrap();
    let reply = HostToAgent::InteractionResponse {
        request_id: "q-1".into(),
        response: resolved,
    };
    write_message(&mut w, &reply).await.unwrap();

    let child_received = child.await.unwrap();
    match child_received {
        HostToAgent::InteractionResponse {
            request_id,
            response,
        } => {
            assert_eq!(request_id, "q-1");
            match response {
                UIInteractionResponse::SelectedOption { request_id, index } => {
                    assert_eq!(request_id, "q-1");
                    assert_eq!(index, 1);
                }
                _ => panic!("expected SelectedOption"),
            }
        }
        _ => panic!("expected InteractionResponse"),
    }
}

#[tokio::test]
async fn test_host_child_response_final() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let child = tokio::spawn(async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (_r, mut w) = stream.into_split();

        let msg = AgentToHost::Response {
            text: "done".into(),
            entry_id: 0,
            is_final: true,
            reply_to_message_id: None,
        };
        write_message(&mut w, &msg).await.unwrap();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut r, _w) = stream.into_split();

    let received: AgentToHost = read_message(&mut r).await.unwrap().unwrap();

    child.await.unwrap();

    match received {
        AgentToHost::Response {
            text,
            entry_id,
            is_final,
            reply_to_message_id,
        } => {
            assert_eq!(text, "done");
            assert_eq!(entry_id, 0);
            assert!(is_final);
            assert_eq!(reply_to_message_id, None);
        }
        _ => panic!("expected Response"),
    }
}
