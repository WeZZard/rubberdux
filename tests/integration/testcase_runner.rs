/// Integration test runner for `.testcase.md` files.
///
/// Discovers `*.testcase.md` files in `tests/integration/cases/`,
/// runs each case through the AgentLoopHarness (direct AgentLoop, no Telegram channel),
/// and evaluates natural-language assertions with the real evaluator LLM.
#[tokio::test(flavor = "multi_thread")]
async fn test_integration_testcases() {
    eprintln!("DEBUG: test_integration_testcases starting");
    crate::support::runner::run().await;
    eprintln!("DEBUG: test_integration_testcases finished");
}
