use rubberdux::agent::external::{QuestionOption, UIInteractionRequest, UIInteractionResponse};
use rubberdux::protocol::{self, AgentToHost, HostToAgent};
use tokio::net::{TcpListener, TcpStream};

/// Verify that an ExternalInteraction message survives a TCP write/read cycle.
#[tokio::test(flavor = "multi_thread")]
async fn test_tcp_write_read_external_interaction() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let writer_task = tokio::spawn(async move {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (_, mut writer) = stream.into_split();
        let msg = AgentToHost::ExternalInteraction {
            task_id: "task-tcp-1".into(),
            request: UIInteractionRequest::Question {
                request_id: "q-tcp-1".into(),
                agent_task_id: "task-tcp-1".into(),
                text: "Which DB?".into(),
                options: vec![QuestionOption {
                    label: "PG".into(),
                    description: "".into(),
                }],
            },
        };
        protocol::write_message(&mut writer, &msg).await.unwrap();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, _) = stream.into_split();
    let received: AgentToHost = protocol::read_message(&mut reader).await.unwrap().unwrap();

    match received {
        AgentToHost::ExternalInteraction { task_id, request } => {
            assert_eq!(task_id, "task-tcp-1");
            match request {
                UIInteractionRequest::Question {
                    request_id, text, ..
                } => {
                    assert_eq!(request_id, "q-tcp-1");
                    assert_eq!(text, "Which DB?");
                }
                _ => panic!("Expected Question"),
            }
        }
        _ => panic!("Expected ExternalInteraction"),
    }

    writer_task.await.unwrap();
}

/// Verify that an InteractionResponse message survives a TCP write/read cycle.
#[tokio::test(flavor = "multi_thread")]
async fn test_tcp_write_read_interaction_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let writer_task = tokio::spawn(async move {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (_, mut writer) = stream.into_split();
        let msg = HostToAgent::InteractionResponse {
            request_id: "q-tcp-2".into(),
            response: UIInteractionResponse::SelectedOption {
                request_id: "q-tcp-2".into(),
                index: 1,
            },
        };
        protocol::write_message(&mut writer, &msg).await.unwrap();
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, _) = stream.into_split();
    let received: HostToAgent = protocol::read_message(&mut reader).await.unwrap().unwrap();

    match received {
        HostToAgent::InteractionResponse {
            request_id,
            response,
        } => {
            assert_eq!(request_id, "q-tcp-2");
            match response {
                UIInteractionResponse::SelectedOption { index, .. } => assert_eq!(index, 1),
                _ => panic!("Expected SelectedOption"),
            }
        }
        _ => panic!("Expected InteractionResponse"),
    }

    writer_task.await.unwrap();
}

/// Verify bidirectional request/response flow: a "child" sends an
/// ExternalInteraction and the "host" replies with an InteractionResponse.
#[tokio::test(flavor = "multi_thread")]
async fn test_tcp_bidirectional_interaction_flow() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // "VM child" connects, sends ExternalInteraction, then reads response
    let child_task = tokio::spawn(async move {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        // Send interaction request
        let msg = AgentToHost::ExternalInteraction {
            task_id: "bidir-1".into(),
            request: UIInteractionRequest::PermissionRequest {
                request_id: "perm-bidir".into(),
                agent_task_id: "bidir-1".into(),
                description: "cargo test".into(),
            },
        };
        protocol::write_message(&mut writer, &msg).await.unwrap();

        // Read response
        let resp: HostToAgent = protocol::read_message(&mut reader).await.unwrap().unwrap();
        match resp {
            HostToAgent::InteractionResponse {
                request_id,
                response,
            } => {
                assert_eq!(request_id, "perm-bidir");
                assert!(matches!(
                    response,
                    UIInteractionResponse::PermissionGranted { .. }
                ));
            }
            _ => panic!("Expected InteractionResponse"),
        }
    });

    // "Host" accepts, reads request, sends response
    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, mut writer) = stream.into_split();

    let received: AgentToHost = protocol::read_message(&mut reader).await.unwrap().unwrap();
    match &received {
        AgentToHost::ExternalInteraction { task_id, .. } => {
            assert_eq!(task_id, "bidir-1");
        }
        _ => panic!("Expected ExternalInteraction"),
    }

    // Send response back
    let resp = HostToAgent::InteractionResponse {
        request_id: "perm-bidir".into(),
        response: UIInteractionResponse::PermissionGranted {
            request_id: "perm-bidir".into(),
        },
    };
    protocol::write_message(&mut writer, &resp).await.unwrap();

    child_task.await.unwrap();
}

/// Verify that multiple sequential interactions (request then response) work
/// correctly over a single TCP connection.
#[tokio::test(flavor = "multi_thread")]
async fn test_tcp_multiple_concurrent_interactions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let child_task = tokio::spawn(async move {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();

        // Send 3 interactions
        for i in 0..3usize {
            let msg = AgentToHost::ExternalInteraction {
                task_id: format!("multi-{}", i),
                request: UIInteractionRequest::Question {
                    request_id: format!("q-multi-{}", i),
                    agent_task_id: format!("multi-{}", i),
                    text: format!("Question {}", i),
                    options: vec![],
                },
            };
            protocol::write_message(&mut writer, &msg).await.unwrap();
        }

        // Read 3 responses
        for i in 0..3usize {
            let resp: HostToAgent = protocol::read_message(&mut reader).await.unwrap().unwrap();
            match resp {
                HostToAgent::InteractionResponse {
                    request_id,
                    response,
                } => {
                    assert_eq!(request_id, format!("q-multi-{}", i));
                    match response {
                        UIInteractionResponse::SelectedOption { index, .. } => {
                            assert_eq!(index, i);
                        }
                        _ => panic!("Expected SelectedOption"),
                    }
                }
                _ => panic!("Expected InteractionResponse"),
            }
        }
    });

    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, mut writer) = stream.into_split();

    // Read 3 requests and send 3 responses
    for i in 0..3usize {
        let received: AgentToHost = protocol::read_message(&mut reader).await.unwrap().unwrap();
        match received {
            AgentToHost::ExternalInteraction { task_id, .. } => {
                assert_eq!(task_id, format!("multi-{}", i));
            }
            _ => panic!("Expected ExternalInteraction"),
        }

        let resp = HostToAgent::InteractionResponse {
            request_id: format!("q-multi-{}", i),
            response: UIInteractionResponse::SelectedOption {
                request_id: format!("q-multi-{}", i),
                index: i,
            },
        };
        protocol::write_message(&mut writer, &resp).await.unwrap();
    }

    child_task.await.unwrap();
}
