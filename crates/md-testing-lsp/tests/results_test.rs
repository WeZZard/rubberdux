use md_testing::TestResults;
use md_testing_lsp::results::ResultsStore;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn reads_suite_scoped_results() {
    let workspace = temp_workspace("suite-scoped-results");
    write_results(
        &workspace.join(
            "tests/results/20260426_120000/integration/new_agent_loop_u2a_1t_greeting/results.json",
        ),
        "new_agent_loop_u2a_1t_greeting",
        "20260426_120000",
        "integration",
        false,
    );

    let store = ResultsStore::new();
    store.init(&workspace).await;

    let results = store
        .get_results("new_agent_loop_u2a_1t_greeting")
        .await
        .expect("should find suite-scoped results");

    assert!(!results.passed);
    assert_eq!(results.run_id, "20260426_120000");
    assert_eq!(results.target, "integration");

    std::fs::remove_dir_all(workspace).unwrap();
}

#[tokio::test]
async fn reads_legacy_flat_results() {
    let workspace = temp_workspace("legacy-flat-results");
    write_results(
        &workspace.join(
            "tests/results/20260426_115959/existing_agent_loop_u2a_mt_session_continuation/results.json",
        ),
        "existing_agent_loop_u2a_mt_session_continuation",
        "20260426_115959",
        "integration",
        true,
    );

    let store = ResultsStore::new();
    store.init(&workspace).await;

    let results = store
        .get_results("existing_agent_loop_u2a_mt_session_continuation")
        .await
        .expect("should find legacy flat results");

    assert!(results.passed);
    assert_eq!(results.run_id, "20260426_115959");

    std::fs::remove_dir_all(workspace).unwrap();
}

fn write_results(path: &Path, testcase_name: &str, run_id: &str, target: &str, passed: bool) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    TestResults {
        testcase_name: testcase_name.to_string(),
        run_id: run_id.to_string(),
        timestamp: "2026-04-26-12-00-00-UTC".to_string(),
        target: target.to_string(),
        passed,
        assertions: Vec::new(),
    }
    .write(path)
    .unwrap();
}

fn temp_workspace(test_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "rubberdux-md-testing-lsp-{}-{}-{}",
        std::process::id(),
        test_name,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}
