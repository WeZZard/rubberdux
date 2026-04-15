mod support;

/// System test for the Telegram channel agent behavior.
///
/// Discovers `testcase_*.md` files in `cases/`, mocks the agent LLM backend
/// with scripted responses, runs each case through the full in-process agent
/// loop, and evaluates natural-language assertions with the real evaluator LLM.
#[tokio::test(flavor = "multi_thread")]
async fn test_telegram_channel_agent() {
    support::runner::run().await;
}
