use std::path::{Path, PathBuf};
use std::time::Duration;

use md_testing::evaluator::{AssertionEvaluator, Evaluatable};
use md_testing::guidance::render_guidance;
use md_testing::llm::{LlmClient, LlmError};
use md_testing::ordering::match_assistant_slots;
use md_testing::{AssistantSlotArtifact, ExchangeFailure, ExecutionArtifact};
use md_testing::{Message, OrderingDirective, UserContent};

use super::harness::{ChannelHarness, Trajectory, build_system_prompt};

/// Run all system test cases from `tests/system/cases/`.
pub async fn run() {
    dotenvy::dotenv().ok();

    ensure_mlx_server().await;

    let llm = MlxLlmClient::from_env();
    let model = std::env::var("MD_TESTING_LLM_MODEL").expect("MD_TESTING_LLM_MODEL must be set");

    let cases_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("system")
        .join("cases");
    let cases = md_testing::discovery::discover_cases(&cases_dir);
    assert!(!cases.is_empty(), "No test cases found in {:?}", cases_dir);

    let run = super::artifacts::system_run();
    let run_dir = run.dir;
    let run_timestamp = run.timestamp;
    let dir_name = run.run_id;
    let system_prompt = build_system_prompt();

    let execution_paths = execute_cases(
        &cases,
        &cases_dir,
        &run_dir,
        &run_timestamp,
        &dir_name,
        &system_prompt,
        &llm,
        &model,
    )
    .await;

    let failed_cases = evaluate_execution_artifacts(&execution_paths, &run_dir, &llm, &model).await;

    if !failed_cases.is_empty() {
        panic!(
            "{} case(s) failed: {:?}. Artifacts: {}",
            failed_cases.len(),
            failed_cases,
            run_dir.display()
        );
    }

    println!(
        "\n=== All {} cases passed. Artifacts: {} ===",
        execution_paths.len(),
        run_dir.display()
    );
}

async fn execute_cases(
    cases: &[md_testing::TestCase],
    cases_dir: &Path,
    run_dir: &Path,
    run_timestamp: &str,
    run_id: &str,
    system_prompt: &str,
    llm: &MlxLlmClient,
    model: &str,
) -> Vec<PathBuf> {
    let mut execution_paths = Vec::new();

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
        let harness = ChannelHarness::new(system_prompt, session_path.clone()).await;

        let timeout = Duration::from_secs(case.front_matter.timeout.max(60));

        let mut user_messages = Vec::new();
        let mut assistant_slots: Vec<(OrderingDirective, Vec<String>)> = Vec::new();
        let mut message_batches: Vec<(usize, Vec<String>)> = Vec::new();
        let mut current_batch: Vec<String> = Vec::new();
        let mut exchange_failures: Vec<ExchangeFailure> = Vec::new();

        for msg in case.messages.iter() {
            match msg {
                Message::User(content) => {
                    let text = match content {
                        UserContent::PlainText(t) => t.clone(),
                        UserContent::Guidance(guidance) => {
                            render_guidance(guidance, llm, model).await
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
                        message_batches
                            .push((user_messages.len().saturating_sub(1), current_batch));
                        current_batch = Vec::new();
                    }
                    assistant_slots.push((directive.clone(), assertions.clone()));
                }
            }
        }

        // Flush any trailing user messages after the last assistant slot.
        if !current_batch.is_empty() {
            message_batches.push((user_messages.len().saturating_sub(1), current_batch));
        }

        let mut all_responses = Vec::new();
        for (last_user_msg_idx, batch) in &message_batches {
            let exchange = if batch.len() == 1 {
                harness.send_message(&batch[0], timeout).await
            } else {
                harness.send_messages_batch(batch, timeout).await
            };
            if let Some(reason) = exchange.failure_reason {
                exchange_failures.push(ExchangeFailure {
                    user_message_index: *last_user_msg_idx,
                    reason,
                });
            }
            all_responses.extend(exchange.responses);
        }

        let trajectory = Trajectory {
            case_name: case.name.clone(),
            test_time: run_timestamp.to_string(),
            user_messages: user_messages.clone(),
            responses: all_responses,
            session_path: session_path.clone(),
        };

        let narration_text = trajectory.format_for_eval();
        md_testing::write_text_atomically(&session_dir.join("main-agent.md"), &narration_text)
            .expect("failed to write narration");

        trajectory.write_subagent_narrations();

        println!("  Received {} response(s)", trajectory.responses.len());
        println!("  Artifacts: {}", case_dir.display());

        // Count actual assistant messages from the session transcript.
        let actual_assistant_count = count_assistant_messages(&session_path);

        let case_content =
            std::fs::read_to_string(cases_dir.join(format!("{}.testcase.md", case.name)))
                .expect("failed to read testcase file");

        let artifact = ExecutionArtifact {
            testcase_name: case.name.clone(),
            run_id: run_id.to_string(),
            timestamp: run_timestamp.to_string(),
            target: case.front_matter.target.clone(),
            case_content,
            trajectory_markdown: narration_text,
            user_messages,
            assistant_slots: assistant_slots
                .into_iter()
                .map(|(directive, assertions)| AssistantSlotArtifact {
                    directive,
                    assertions,
                })
                .collect(),
            actual_assistant_count,
            exchange_failures,
        };

        let execution_path = case_dir.join("execution.json");
        artifact
            .write(&execution_path)
            .expect("failed to write execution.json");
        execution_paths.push(execution_path);
    }

    execution_paths
}

async fn evaluate_execution_artifacts(
    execution_paths: &[PathBuf],
    run_dir: &Path,
    llm: &MlxLlmClient,
    model: &str,
) -> Vec<String> {
    let evaluator = AssertionEvaluator::new(llm.clone())
        .with_model(model)
        .with_consistency_votes(3);
    let mut artifacts: Vec<ExecutionArtifact> = execution_paths
        .iter()
        .map(|path| ExecutionArtifact::read(path).expect("failed to read execution.json"))
        .collect();
    artifacts.sort_by(|a, b| a.testcase_name.cmp(&b.testcase_name));

    let mut failed_cases: Vec<String> = Vec::new();

    for artifact in artifacts {
        println!("\n=== Evaluating case: {} ===", artifact.testcase_name);

        let case_dir = run_dir.join(&artifact.testcase_name);
        let case = md_testing::parser::parse(&artifact.case_content, &artifact.testcase_name)
            .expect("failed to parse execution testcase content");
        let trajectory = ExecutionTrajectory {
            markdown: &artifact.trajectory_markdown,
        };
        let directives: Vec<OrderingDirective> = artifact
            .assistant_slots
            .iter()
            .map(|slot| slot.directive.clone())
            .collect();

        let mut eval_results = String::new();
        let mut all_passed = true;
        let mut storyline_results = Vec::new();
        let mut assistant_results = Vec::new();

        for failure in &artifact.exchange_failures {
            eval_results.push_str(&format!(
                "## User Message {} Exchange\n",
                failure.user_message_index
            ));
            eval_results.push_str("- Passed: false\n");
            eval_results.push_str(&format!("- Reasoning: {}\n\n", failure.reason));
            println!(
                "  User Message {} exchange failed: {}",
                failure.user_message_index, failure.reason
            );
            all_passed = false;
        }

        // Run ordering match first.
        let (matched_indices, ordering_error) =
            match match_assistant_slots(&directives, artifact.actual_assistant_count) {
                Ok(indices) => (indices, None),
                Err(e) => {
                    eval_results.push_str(&format!("## Ordering Match Error\n\n{}\n\n", e));
                    println!("  Ordering match failed: {}", e);
                    all_passed = false;
                    (Vec::new(), Some(e))
                }
            };

        for assertion in &case.storyline {
            let result = evaluator.evaluate_storyline(&trajectory, assertion).await;

            eval_results.push_str(&format!("## Storyline: {}\n", assertion));
            eval_results.push_str(&format!("- Passed: {}\n", result.passed));
            append_evaluation_timing(&mut eval_results, &result);
            eval_results.push_str(&format!("- Reasoning: {}\n\n", result.reasoning));

            println!("  Storyline: {}", assertion);
            println!("    Passed: {}", result.passed);
            println!("    Duration: {} ms", result.duration_ms);
            println!("    Reasoning: {}", result.reasoning);

            if !result.passed {
                all_passed = false;
            }
            storyline_results.push((assertion.clone(), result));
        }

        for (slot_idx, slot) in artifact.assistant_slots.iter().enumerate() {
            let actual_idx = matched_indices.get(slot_idx).copied();
            for assertion in &slot.assertions {
                let result = if let Some(idx) = actual_idx {
                    evaluator
                        .evaluate_assistant(&trajectory, assertion, idx)
                        .await
                } else {
                    md_testing::evaluator::EvaluationResult {
                        passed: false,
                        reasoning: "Could not match assistant message — ordering match failed"
                            .to_string(),
                        duration_ms: 0,
                        llm_calls: 0,
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
                append_evaluation_timing(&mut eval_results, &result);
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
                println!("    Duration: {} ms", result.duration_ms);
                println!("    Reasoning: {}", result.reasoning);

                if !result.passed {
                    all_passed = false;
                }
                assistant_results.push((slot_idx, actual_idx, assertion.clone(), result));
            }
        }

        md_testing::write_text_atomically(&case_dir.join("evaluation.md"), &eval_results)
            .expect("failed to write evaluation");

        // Write machine-readable results.json for LSP
        let case_content = &artifact.case_content;
        let assertion_lines = md_testing::map_assertion_lines(case_content, &case);
        let mut results_assertions = Vec::new();

        // User-message exchange failures
        let user_lines = md_testing::find_user_heading_lines(case_content);
        for failure in &artifact.exchange_failures {
            results_assertions.push(md_testing::AssertionResult {
                scope: md_testing::AssertionScope::UserMessage {
                    msg_index: failure.user_message_index,
                },
                line: user_lines
                    .get(failure.user_message_index)
                    .copied()
                    .unwrap_or(1),
                assertion: "Message exchange completed with a final assistant response".to_string(),
                passed: false,
                reasoning: failure.reason.clone(),
                evaluation_duration_ms: None,
                evaluator_call_count: None,
            });
        }

        // Storyline assertions
        let storyline_line = md_testing::find_heading_line(case_content, "Storyline").unwrap_or(1);
        for (assertion, result) in &storyline_results {
            let line = assertion_lines
                .iter()
                .find(|l| l.msg_index == 0 && l.assertion == *assertion)
                .map(|l| l.line)
                .unwrap_or(storyline_line);
            results_assertions.push(md_testing::AssertionResult {
                scope: md_testing::AssertionScope::Storyline,
                line,
                assertion: assertion.clone(),
                passed: result.passed,
                reasoning: result.reasoning.clone(),
                evaluation_duration_ms: evaluation_duration_ms(result),
                evaluator_call_count: evaluator_call_count(result),
            });
        }

        // Ordering match result
        let ordering_line = md_testing::find_assistant_heading_lines(case_content)
            .first()
            .copied()
            .unwrap_or(1);
        if let Some(e) = &ordering_error {
            results_assertions.push(md_testing::AssertionResult {
                scope: md_testing::AssertionScope::OrderingMatch,
                line: ordering_line,
                assertion: "Ordering match".to_string(),
                passed: false,
                reasoning: e.to_string(),
                evaluation_duration_ms: None,
                evaluator_call_count: None,
            });
        }

        // Assistant assertions
        let assistant_lines = md_testing::find_assistant_heading_lines(case_content);
        for (slot_idx, actual_idx, assertion, result) in &assistant_results {
            let heading_line = assistant_lines.get(*slot_idx).copied().unwrap_or(1);
            let line = assertion_lines
                .iter()
                .find(|l| l.msg_index == *slot_idx && l.assertion == *assertion)
                .map(|l| l.line)
                .unwrap_or(heading_line);
            results_assertions.push(md_testing::AssertionResult {
                scope: md_testing::AssertionScope::AssistantMessage {
                    slot_index: *slot_idx,
                    actual_index: *actual_idx,
                },
                line,
                assertion: assertion.clone(),
                passed: result.passed,
                reasoning: result.reasoning.clone(),
                evaluation_duration_ms: evaluation_duration_ms(result),
                evaluator_call_count: evaluator_call_count(result),
            });
        }

        let test_results = md_testing::TestResults {
            testcase_name: artifact.testcase_name.clone(),
            run_id: artifact.run_id.clone(),
            timestamp: artifact.timestamp.clone(),
            target: artifact.target.clone(),
            passed: all_passed,
            assertions: results_assertions,
        };
        test_results
            .write(&case_dir.join("results.json"))
            .expect("failed to write results.json");

        if !all_passed {
            eprintln!(
                "\nFAILED: One or more assertions failed for case '{}'. See {}",
                artifact.testcase_name,
                case_dir.display()
            );
            failed_cases.push(artifact.testcase_name.clone());
        }
    }

    failed_cases
}

fn append_evaluation_timing(output: &mut String, result: &md_testing::EvaluationResult) {
    if result.llm_calls > 0 {
        output.push_str(&format!(
            "- Evaluation Duration: {} ms\n",
            result.duration_ms
        ));
        output.push_str(&format!("- Evaluator Calls: {}\n", result.llm_calls));
    }
}

fn evaluation_duration_ms(result: &md_testing::EvaluationResult) -> Option<u64> {
    (result.llm_calls > 0).then_some(result.duration_ms)
}

fn evaluator_call_count(result: &md_testing::EvaluationResult) -> Option<usize> {
    (result.llm_calls > 0).then_some(result.llm_calls)
}

struct ExecutionTrajectory<'a> {
    markdown: &'a str,
}

impl Evaluatable for ExecutionTrajectory<'_> {
    fn format_for_eval(&self) -> String {
        self.markdown.to_string()
    }
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

/// Check if MLX server is running, start it if not.
async fn ensure_mlx_server() {
    let base_url =
        std::env::var("MD_TESTING_LLM_BASE_URL").expect("MD_TESTING_LLM_BASE_URL must be set");

    // Check if server is already running
    if reqwest::Client::new()
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
    {
        return;
    }

    println!("MLX server not running, starting it...");

    let model = std::env::var("MD_TESTING_LLM_MODEL").expect("MD_TESTING_LLM_MODEL must be set");

    let port = mlx_server_port(&base_url);

    // Start MLX server in background (don't store Child so it outlives the test)
    let mut cmd = std::process::Command::new("python3.11");
    cmd.args(["-m", "mlx_lm.server", "--model", &model, "--port", &port])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let _ = cmd.spawn().expect("Failed to start MLX server");

    // Wait for server to be ready
    let client = reqwest::Client::new();
    let mut attempts = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if client
            .get(format!("{}/models", base_url.trim_end_matches('/')))
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
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

fn mlx_server_port(base_url: &str) -> String {
    let without_scheme = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    authority
        .rsplit_once(':')
        .map(|(_, port)| port.to_string())
        .unwrap_or_else(|| "8080".to_string())
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
