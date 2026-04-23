use std::sync::Arc;
use std::time::Duration;

use rubberdux::provider::moonshot::tool::{ToolCall, FunctionCall};
use rubberdux::tool::ToolRegistry;

use crate::support::mock_tools::MockTool;

/// Test that dependent tools execute in topological order (waves).
#[tokio::test]
async fn test_dependent_tools_execute_in_order() {
    // Create tools with measurable delays
    let tool_a = MockTool::new("tool_a", Duration::from_millis(50));
    let tool_b = MockTool::new("tool_b", Duration::from_millis(30));
    let tool_c = MockTool::new("tool_c", Duration::from_millis(20));

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(tool_a.clone()));
    registry.register(Box::new(tool_b.clone()));
    registry.register(Box::new(tool_c.clone()));

    // Tool A: no dependency
    // Tool B: depends on A
    // Tool C: depends on B
    let calls = vec![
        ToolCall {
            index: None,
            id: "call_a".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_a".into(),
                arguments: "{}".into(),
            },
            depends_on: None,
        },
        ToolCall {
            index: None,
            id: "call_b".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_b".into(),
                arguments: "{}".into(),
            },
            depends_on: Some("call_a".into()),
        },
        ToolCall {
            index: None,
            id: "call_c".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_c".into(),
                arguments: "{}".into(),
            },
            depends_on: Some("call_b".into()),
        },
    ];

    let start = std::time::Instant::now();
    let results = rubberdux::agent::runtime::turn_driver::execute_tool_calls(
        &calls, &registry
    ).await;
    let elapsed = start.elapsed();

    assert_eq!(results.len(), 3);

    // Verify execution order: A before B before C
    let order_a = results.iter().position(|(call, _)| call.id == "call_a").unwrap();
    let order_b = results.iter().position(|(call, _)| call.id == "call_b").unwrap();
    let order_c = results.iter().position(|(call, _)| call.id == "call_c").unwrap();

    assert!(order_a < order_b, "A should execute before B");
    assert!(order_b < order_c, "B should execute before C");

    // Timing: sequential waves should take ~100ms (50+30+20)
    // Concurrent execution would take ~50ms (max of all)
    assert!(
        elapsed >= Duration::from_millis(80),
        "Sequential waves should take at least 80ms, took {:?}",
        elapsed
    );
}

/// Test that independent tools execute concurrently.
#[tokio::test]
async fn test_independent_tools_execute_concurrently() {
    let tool_a = MockTool::new("tool_a", Duration::from_millis(50));
    let tool_b = MockTool::new("tool_b", Duration::from_millis(50));

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(tool_a));
    registry.register(Box::new(tool_b));

    let calls = vec![
        ToolCall {
            index: None,
            id: "call_a".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_a".into(),
                arguments: "{}".into(),
            },
            depends_on: None,
        },
        ToolCall {
            index: None,
            id: "call_b".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_b".into(),
                arguments: "{}".into(),
            },
            depends_on: None,
        },
    ];

    let start = std::time::Instant::now();
    let results = rubberdux::agent::runtime::turn_driver::execute_tool_calls(
        &calls, &registry
    ).await;
    let elapsed = start.elapsed();

    assert_eq!(results.len(), 2);

    // Concurrent execution should take ~50ms, not ~100ms
    assert!(
        elapsed < Duration::from_millis(90),
        "Independent tools should run concurrently, took {:?}",
        elapsed
    );
}

/// Test that cyclic dependencies are detected and handled.
#[tokio::test]
async fn test_cyclic_dependency_returns_error() {
    let tool_a = MockTool::new("tool_a", Duration::from_millis(10));

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(tool_a));

    // A depends on C, B depends on A, C depends on B (cycle)
    let calls = vec![
        ToolCall {
            index: None,
            id: "call_a".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_a".into(),
                arguments: "{}".into(),
            },
            depends_on: Some("call_c".into()),
        },
        ToolCall {
            index: None,
            id: "call_b".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_a".into(),
                arguments: "{}".into(),
            },
            depends_on: Some("call_a".into()),
        },
        ToolCall {
            index: None,
            id: "call_c".into(),
            r#type: "function".into(),
            function: FunctionCall {
                name: "tool_a".into(),
                arguments: "{}".into(),
            },
            depends_on: Some("call_b".into()),
        },
    ];

    let results = rubberdux::agent::runtime::turn_driver::execute_tool_calls(
        &calls, &registry
    ).await;

    assert_eq!(results.len(), 3);

    // All results should be errors due to cyclic dependency
    for (_, outcome) in &results {
        match outcome {
            rubberdux::tool::ToolOutcome::Immediate { is_error, .. } => {
                assert!(*is_error, "Cyclic dependency should return error");
            }
            _ => panic!("Expected Immediate error outcome for cyclic dependency"),
        }
    }
}
