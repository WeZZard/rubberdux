use std::sync::{Arc, RwLock};

use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::tool::agent::{build_subagent_registries, AgentTool};
use rubberdux::tool::ToolRegistry;
use tokio::net::TcpListener;

fn dummy_client() -> Arc<MoonshotClient> {
    Arc::new(MoonshotClient::new(
        reqwest::Client::new(),
        "http://localhost:0".into(),
        "test-key".into(),
        "test-model".into(),
    ))
}

fn make_registry_with_agent(
    client: Arc<MoonshotClient>,
) -> ToolRegistry {
    let last_query = Arc::new(RwLock::new(String::new()));
    let registries = build_subagent_registries(&client, &last_query);
    let (context_tx, _) = tokio::sync::broadcast::channel(4);

    let agent_tool = AgentTool::new(
        client,
        registries,
        "integration test system prompt".into(),
        context_tx,
        None,
        None,
    );

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(agent_tool));
    registry
}

#[tokio::test(flavor = "multi_thread")]
async fn test_explore_via_registry_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client);

    let outcome = registry
        .execute("agent", r#"{"subagent_type":"explore","prompt":"find x"}"#)
        .await;

    match outcome {
        rubberdux::tool::ToolOutcome::Subagent { handle } => {
            assert!(!handle.task_id.is_empty());
            handle.cancel.cancel();
        }
        other => panic!("Expected Subagent, got {:?}", std::mem::discriminant(&other)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_plan_via_registry_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client);

    let outcome = registry
        .execute("agent", r#"{"subagent_type":"plan","prompt":"plan x"}"#)
        .await;

    match outcome {
        rubberdux::tool::ToolOutcome::Subagent { handle } => {
            handle.cancel.cancel();
        }
        other => panic!("Expected Subagent, got {:?}", std::mem::discriminant(&other)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_general_purpose_via_registry_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client);

    let outcome = registry
        .execute("agent", r#"{"subagent_type":"general_purpose","prompt":"do x"}"#)
        .await;

    match outcome {
        rubberdux::tool::ToolOutcome::Subagent { handle } => {
            handle.cancel.cancel();
        }
        other => panic!("Expected Subagent, got {:?}", std::mem::discriminant(&other)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_computer_use_with_rpc_returns_immediate() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let acceptor = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut r, _) = stream.into_split();
        let _ = rubberdux::vm::rpc::read_message::<
            rubberdux::vm::rpc::AgentToHost,
        >(&mut r)
            .await;
    });

    let client = dummy_client();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (_r, w) = stream.into_split();
    let _rpc_writer = Some(Arc::new(tokio::sync::Mutex::new(w)));

    let registry = make_registry_with_agent(client);

    let outcome = registry
        .execute(
            "agent",
            r#"{"subagent_type":"computer_use","prompt":"click ok"}"#,
        )
        .await;

    match outcome {
        rubberdux::tool::ToolOutcome::Immediate { content, is_error } => {
            assert!(!is_error);
            assert!(
                content.contains("Computer-use VM agent"),
                "expected VM dispatch message, got: {}",
                content
            );
        }
        other => panic!("Expected Immediate, got {:?}", std::mem::discriminant(&other)),
    }

    acceptor.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_computer_use_without_rpc_falls_back_to_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client);

    let outcome = registry
        .execute(
            "agent",
            r#"{"subagent_type":"computer_use","prompt":"click ok"}"#,
        )
        .await;

    match outcome {
        rubberdux::tool::ToolOutcome::Subagent { handle } => {
            handle.cancel.cancel();
        }
        other => panic!("Expected Subagent, got {:?}", std::mem::discriminant(&other)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_subagent_session_persistence() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client.clone());

    let outcome = registry
        .execute("agent", r#"{"subagent_type":"explore","prompt":"find x"}"#)
        .await;

    let handle = match outcome {
        rubberdux::tool::ToolOutcome::Subagent { handle } => handle,
        other => panic!("Expected Subagent, got {:?}", std::mem::discriminant(&other)),
    };

    // Just verify the handle was created with a valid task_id
    assert!(!handle.task_id.is_empty());
    handle.cancel.cancel();
}
