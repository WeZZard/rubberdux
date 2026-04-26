use md_testing_lsp::results::ResultsStore;
use std::path::Path;

#[tokio::test]
async fn test_results_store_reads_existing_results() {
    let store = ResultsStore::new();
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    store.init(workspace_root).await;

    // Give it time to scan
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Check if we can get results for a known test case
    let results = store
        .get_results("existing_agent_loop_u2a_mt_session_continuation")
        .await;
    assert!(results.is_some(), "Should find existing test results");

    let results = results.unwrap();
    assert!(results.passed);
    assert_eq!(results.assertions.len(), 7);
}
