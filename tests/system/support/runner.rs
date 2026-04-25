use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use md_testing::evaluator::{AssertionEvaluator, Evaluatable};
use md_testing::guidance::render_guidance;
use md_testing::llm::{LlmClient, LlmError};
use md_testing::narration;
use md_testing::ordering::{MatchError, match_assistant_slots};
use md_testing::{Message, OrderingDirective, UserContent};

use super::harness::{ChannelHarness, Trajectory, build_system_prompt};

/// Run all system test cases from `tests/system/cases/`.
pub async fn run() {
    dotenvy::dotenv().ok();

    ensure_mlx_server().await;

    let llm = MlxLlmClient::from_env();
    let model = std::env::var("MD_TESTING_LLM_MODEL").expect("MD_TESTING_LLM_MODEL must be set");

    let evaluator = AssertionEvaluator::new(llm.clone())
        .with_model(&model)
        .with_consistency_votes(3);

    let cases_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("system")
        .join("cases");
    let cases = md_testing::discovery::discover_cases(&cases_dir);
    assert!(!cases.is_empty(), "No test cases found in {:?}", cases_dir);

    let (run_dir, run_timestamp) = artifact_run_dir();
    let dir_name = run_dir.file_name().unwrap().to_string_lossy().to_string();
    let system_prompt = build_system_prompt();

    for case in cases {
        if case.front_matter.target != "telegram-channel" {
            continue;
        }

        // Skip tests that require VM features (not supported by ChannelHarness)
        if case.front_matter.features.contains(&"vm".to_string()) {
            println!("\n=== Skipping case: {} (requires VM) ===", case.name);
            continue;
        }

        println!("\n=== Running case: {} ===", case.name);

        let case_dir = run_dir.join(&case.name);
        std::fs::create_dir_all(&case_dir).expect("failed to create artifact dir");

        let session_dir = case_dir.join("session");
        std::fs::create_dir_all(&session_dir).expect("failed to create session dir");
        let session_path = session_dir.join("main-agent.jsonl");
        let harness = ChannelHarness::new(&system_prompt, session_path.clone()).await;

        let timeout = Duration::from_secs(case.front_matter.timeout.max(60));

        let mut user_messages = Vec::new();
        let mut assistant_slots: Vec<(OrderingDirective, Vec<String>)> = Vec::new();
        let mut message_batches: Vec<Vec<String>> = Vec::new();
        let mut current_batch: Vec<String> = Vec::new();

        for msg in case.messages.iter() {
            match msg {
                Message::User(content) => {
                    let text = match content {
                        UserContent::PlainText(t) => t.clone(),
                        UserContent::Guidance(guidance) => {
                            render_guidance(guidance, &llm, &model).await
                        }
                    };
                    user_messages.push(text.clone());
                    current_batch.push(text);
                }
                Message::Assistant {
                    directive,
                    assertions,
                } => {
                    // End of a user-message batch; flush it.
                    if !current_batch.is_empty() {
                        message_batches.push(current_batch);
                        current_batch = Vec::new();
                    }
                    assistant_slots.push((directive.clone(), assertions.clone()));
                }
            }
        }

        // Flush any trailing user messages after the last assistant slot.
        if !current_batch.is_empty() {
            message_batches.push(current_batch);
        }

        let mut all_responses = Vec::new();
        for batch in &message_batches {
            let responses = if batch.len() == 1 {
                harness.send_message(&batch[0], timeout).await
            } else {
                harness.send_messages_batch(batch, timeout).await
            };
            all_responses.extend(responses);
        }

        let trajectory = Trajectory {
            case_name: case.name.clone(),
            test_time: run_timestamp.clone(),
            user_messages: user_messages.iter().map(|t| t.clone()).collect(),
            responses: all_responses,
            session_path: session_path.clone(),
        };

        let narration_text = trajectory.format_for_eval();
        std::fs::write(session_dir.join("main-agent.md"), &narration_text)
            .expect("failed to write narration");

        trajectory.write_subagent_narrations();

        println!("  Received {} response(s)", trajectory.responses.len());
        println!("  Artifacts: {}", case_dir.display());

        // Count actual assistant messages from the session transcript.
        let actual_assistant_count = count_assistant_messages(&session_path);
        let directives: Vec<OrderingDirective> =
            assistant_slots.iter().map(|(d, _)| d.clone()).collect();

        let mut eval_results = String::new();
        let mut all_passed = true;

        // Run ordering match first.
        let matched_indices = match match_assistant_slots(&directives, actual_assistant_count) {
            Ok(indices) => indices,
            Err(e) => {
                eval_results.push_str(&format!("## Ordering Match Error\n\n{}\n\n", e));
                println!("  Ordering match failed: {}", e);
                all_passed = false;
                Vec::new()
            }
        };

        for assertion in &case.storyline {
            let result = evaluator.evaluate_storyline(&trajectory, assertion).await;

            eval_results.push_str(&format!("## Storyline: {}\n", assertion));
            eval_results.push_str(&format!("- Passed: {}\n", result.passed));
            eval_results.push_str(&format!("- Reasoning: {}\n\n", result.reasoning));

            println!("  Storyline: {}", assertion);
            println!("    Passed: {}", result.passed);
            println!("    Reasoning: {}", result.reasoning);

            if !result.passed {
                all_passed = false;
            }
        }

        for (slot_idx, (_, assertions)) in assistant_slots.iter().enumerate() {
            let actual_idx = matched_indices.get(slot_idx).copied();
            for assertion in assertions {
                let result = if let Some(idx) = actual_idx {
                    evaluator
                        .evaluate_assistant(&trajectory, assertion, idx)
                        .await
                } else {
                    md_testing::evaluator::EvaluationResult {
                        passed: false,
                        reasoning: "Could not match assistant message — ordering match failed"
                            .to_string(),
                    }
                };
                eval_results.push_str(&format!(
                    "## Assistant Message {} (slot {}){}\n",
                    actual_idx
                        .map(|i| format!("{}", i))
                        .unwrap_or_else(|| "?".to_string()),
                    slot_idx,
                    if actual_idx.is_none() {
                        " [UNMATCHED]"
                    } else {
                        ""
                    }
                ));
                eval_results.push_str(&format!("Assertion: {}\n", assertion));
                eval_results.push_str(&format!("- Passed: {}\n", result.passed));
                eval_results.push_str(&format!("- Reasoning: {}\n\n", result.reasoning));

                println!(
                    "  Assistant Message {} (slot {}): {}",
                    actual_idx
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    slot_idx,
                    assertion
                );
                println!("    Passed: {}", result.passed);
                println!("    Reasoning: {}", result.reasoning);

                if !result.passed {
                    all_passed = false;
                }
            }
        }

        std::fs::write(case_dir.join("evaluation.md"), &eval_results)
            .expect("failed to write evaluation");

        // Write machine-readable results.json for LSP
        let case_content =
            std::fs::read_to_string(cases_dir.join(format!("{}.testcase.md", case.name)))
                .unwrap_or_default();
        let assertion_lines = md_testing::map_assertion_lines(&case_content, &case);
        let mut results_assertions = Vec::new();

        // Storyline assertions
        let storyline_line = md_testing::find_heading_line(&case_content, "Storyline").unwrap_or(1);
        for assertion in &case.storyline {
            let line = assertion_lines
                .iter()
                .find(|l| l.msg_index == 0 && l.assertion == *assertion)
                .map(|l| l.line)
                .unwrap_or(storyline_line);
            let result = evaluator.evaluate_storyline(&trajectory, assertion).await;
            results_assertions.push(md_testing::AssertionResult {
                scope: md_testing::AssertionScope::Storyline,
                line,
                assertion: assertion.clone(),
                passed: result.passed,
                reasoning: result.reasoning,
            });
        }

        // Ordering match result
        let ordering_line = md_testing::find_assistant_heading_lines(&case_content)
            .first()
            .copied()
            .unwrap_or(1);
        if let Err(ref e) = match_assistant_slots(&directives, actual_assistant_count) {
            results_assertions.push(md_testing::AssertionResult {
                scope: md_testing::AssertionScope::OrderingMatch,
                line: ordering_line,
                assertion: "Ordering match".to_string(),
                passed: false,
                reasoning: e.to_string(),
            });
        }

        // Assistant assertions
        let assistant_lines = md_testing::find_assistant_heading_lines(&case_content);
        for (slot_idx, (_, assertions)) in assistant_slots.iter().enumerate() {
            let actual_idx = matched_indices.get(slot_idx).copied();
            let heading_line = assistant_lines.get(slot_idx).copied().unwrap_or(1);
            for assertion in assertions {
                let result = if let Some(idx) = actual_idx {
                    evaluator
                        .evaluate_assistant(&trajectory, assertion, idx)
                        .await
                } else {
                    md_testing::evaluator::EvaluationResult {
                        passed: false,
                        reasoning: "Could not match assistant message — ordering match failed"
                            .to_string(),
                    }
                };
                results_assertions.push(md_testing::AssertionResult {
                    scope: md_testing::AssertionScope::AssistantMessage {
                        slot_index: slot_idx,
                        actual_index: actual_idx,
                    },
                    line: heading_line,
                    assertion: assertion.clone(),
                    passed: result.passed,
                    reasoning: result.reasoning,
                });
            }
        }

        let test_results = md_testing::TestResults {
            testcase_name: case.name.clone(),
            run_id: dir_name.clone(),
            timestamp: run_timestamp.clone(),
            target: case.front_matter.target.clone(),
            passed: all_passed,
            assertions: results_assertions,
        };
        test_results
            .write(&case_dir.join("results.json"))
            .expect("failed to write results.json");

        if !all_passed {
            eprintln!(
                "\nFAILED: One or more assertions failed for case '{}'. See {}",
                case.name,
                case_dir.display()
            );
        }
    }

    println!(
        "\n=== All cases completed. Artifacts: {} ===",
        run_dir.display()
    );
}

/// LlmClient implementation for md-testing using MLX/local LLM env vars.
#[derive(Clone)]
struct MlxLlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl MlxLlmClient {
    fn from_env() -> Self {
        let base_url =
            std::env::var("MD_TESTING_LLM_BASE_URL").expect("MD_TESTING_LLM_BASE_URL must be set");
        let api_key =
            std::env::var("MD_TESTING_LLM_API_KEY").expect("MD_TESTING_LLM_API_KEY must be set");

        let mut builder = reqwest::ClientBuilder::new();
        if let Ok(user_agent) = std::env::var("MD_TESTING_LLM_USER_AGENT") {
            builder = builder.user_agent(user_agent);
        }
        let http = builder.build().expect("failed to build HTTP client");

        Self {
            http,
            base_url,
            api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

impl LlmClient for MlxLlmClient {
    fn chat_raw(
        &self,
        body: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, LlmError>> + Send + '_>>
    {
        let url = self.url("/chat/completions");
        let auth = self.auth_header();
        Box::pin(async move {
            let response = self
                .http
                .post(&url)
                .header("Authorization", auth)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| LlmError::Request(e.to_string()))?
                .text()
                .await
                .map_err(|e| LlmError::Request(e.to_string()))?;
            Ok(response)
        })
    }
}

/// Create the artifact directory for this test run.
fn artifact_run_dir() -> (PathBuf, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let days = secs / 86400;
    let (year, month, day) = days_to_ymd(days);
    let timestamp = format!(
        "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}-UTC",
        year, month, day, hours, minutes, seconds
    );
    let dir_name = format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}-system",
        year, month, day, hours, minutes, seconds
    );
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("results")
        .join(dir_name);
    std::fs::create_dir_all(&dir).expect("failed to create results dir");
    (dir, timestamp)
}

/// Check if MLX server is running, start it if not.
async fn ensure_mlx_server() {
    let base_url =
        std::env::var("MD_TESTING_LLM_BASE_URL").expect("MD_TESTING_LLM_BASE_URL must be set");

    // Check if server is already running
    if reqwest::Client::new()
        .get(format!("{}/v1/models", base_url.trim_end_matches('/')))
        .send()
        .await
        .is_ok()
    {
        return;
    }

    println!("MLX server not running, starting it...");

    let model = std::env::var("MD_TESTING_LLM_MODEL").expect("MD_TESTING_LLM_MODEL must be set");

    let port = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .nth(1)
        .unwrap_or("8080");

    // Start MLX server in background (don't store Child so it outlives the test)
    let mut cmd = std::process::Command::new("python3.11");
    cmd.args(["-m", "mlx_lm.server", "--model", &model, "--port", port])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let _ = cmd.spawn().expect("Failed to start MLX server");

    // Wait for server to be ready
    let client = reqwest::Client::new();
    let mut attempts = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if client
            .get(format!("{}/v1/models", base_url.trim_end_matches('/')))
            .send()
            .await
            .is_ok()
        {
            println!("MLX server ready");
            break;
        }
        attempts += 1;
        if attempts > 60 {
            panic!("MLX server failed to start within 60 seconds");
        }
    }
}

/// Count assistant messages in a session JSONL file.
fn count_assistant_messages(session_path: &std::path::Path) -> usize {
    let content = match std::fs::read_to_string(session_path) {
        Ok(c) => c,
        Err(_) => return 0,
    };

    let mut count = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(role) = entry["message"].get("role").and_then(|r| r.as_str()) {
                if role == "assistant" {
                    count += 1;
                }
            }
        }
    }
    count
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    let mut days = days_since_epoch;
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(y: u64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}
