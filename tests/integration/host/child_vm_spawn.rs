use std::sync::{Arc, RwLock};

use rubberdux::provider::moonshot::MoonshotClient;
use rubberdux::tool::agent::{build_subagent_registries, AgentTool};
use rubberdux::tool::{SubagentType, ToolRegistry};
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
    rpc_writer: Option<Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>>,
) -> ToolRegistry {
    let last_query = Arc::new(RwLock::new(String::new()));
    let registries = build_subagent_registries(&client, &last_query);
    let (context_tx, _) = tokio::sync::broadcast::channel(4);

    let agent_tool = AgentTool::new(
        client,
        registries,
        "integration test system prompt".into(),
        context_tx,
        rpc_writer,
        None,
    );

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(agent_tool));
    registry
}

/// Test that ComputerUse with RPC writer sends SpawnVM message.
#[tokio::test(flavor = "multi_thread")]
async fn test_computer_use_with_rpc_sends_spawn_vm() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept and read the SpawnVM message
    let acceptor = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut r, _) = stream.into_split();
        let msg = rubberdux::protocol::read_message::<
            rubberdux::protocol::AgentToHost,
        >(&mut r
        )
        .await
        .unwrap()
        .unwrap();

        match msg {
            rubberdux::protocol::AgentToHost::SpawnVM {
                task_id,
                prompt,
                subagent_type,
            } => {
                assert!(!task_id.is_empty());
                assert_eq!(prompt, "click ok");
                assert_eq!(subagent_type, "computer_use");
            }
            other => panic!("Expected SpawnVM, got {:?}", other),
        }
    });

    let client = dummy_client();
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (_r, w) = stream.into_split();
    let rpc_writer = Some(Arc::new(tokio::sync::Mutex::new(w)));

    let registry = make_registry_with_agent(client, rpc_writer);

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

    acceptor.await.unwrap();
}

/// Test that ComputerUse without RPC writer falls back to subagent.
#[tokio::test(flavor = "multi_thread")]
async fn test_computer_use_without_rpc_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client, None);

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

/// Test that Explore subagent always returns Subagent outcome.
#[tokio::test(flavor = "multi_thread")]
async fn test_explore_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client, None);

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

/// Test that Plan subagent always returns Subagent outcome.
#[tokio::test(flavor = "multi_thread")]
async fn test_plan_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client, None);

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

/// Test that GeneralPurpose subagent always returns Subagent outcome.
#[tokio::test(flavor = "multi_thread")]
async fn test_general_purpose_returns_subagent() {
    let client = dummy_client();
    let registry = make_registry_with_agent(client, None);

    let outcome = registry
        .execute(
            "agent",
            r#"{"subagent_type":"general_purpose","prompt":"do x"}"#,
        )
        .await;

    match outcome {
        rubberdux::tool::ToolOutcome::Subagent { handle } => {
            handle.cancel.cancel();
        }
        other => panic!("Expected Subagent, got {:?}", std::mem::discriminant(&other)),
    }
}
