use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Run all system test cases.
pub async fn run() {
    dotenvy::dotenv().ok();

    if std::env::var("RUBBERDUX_LLM_API_KEY")
        .unwrap_or_default()
        .is_empty()
    {
        eprintln!("Skipping system tests: RUBBERDUX_LLM_API_KEY is not set.");
        return;
    }

    let system_prompt = super::harness::build_system_prompt();
    let evaluator = super::evaluator::AssertionEvaluator::from_env();

    let cases_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/system/cases");
    let cases = super::case::discover_cases(&cases_dir);
    assert!(!cases.is_empty(), "No test cases found in {:?}", cases_dir);

    let (run_dir, run_timestamp) = artifact_run_dir();

    for case in cases {
        println!("\n=== Running case: {} ===", case.name);

        let case_dir = run_dir.join(&case.name);
        std::fs::create_dir_all(&case_dir).expect("failed to create artifact dir");

        let session_dir = case_dir.join("session");
        std::fs::create_dir_all(&session_dir).expect("failed to create session dir");
        let session_path = session_dir.join("main-agent.jsonl");
        let harness =
            super::harness::ChannelHarness::new(&system_prompt, session_path.clone()).await;

        let timeout = match case.name.as_str() {
            "testcase_subagent_search" => Duration::from_secs(120),
            _ => Duration::from_secs(60),
        };

        let responses = harness.send_message(&case.user_message, timeout).await;

        let trajectory = super::harness::Trajectory {
            case_name: case.name.clone(),
            test_time: run_timestamp.clone(),
            user_message: case.user_message.clone(),
            responses,
            session_path: session_path.clone(),
        };

        // Write main agent narration into session dir
        let narration_text = trajectory.format_for_eval();
        std::fs::write(session_dir.join("main-agent.md"), &narration_text)
            .expect("failed to write narration");

        // Write subagent narrations
        trajectory.write_subagent_narrations();

        println!("  Received {} response(s)", trajectory.responses.len());
        println!("  Artifacts: {}", case_dir.display());

        // Evaluate assertions and write evaluation.md
        let mut eval_results = String::new();
        let mut all_passed = true;

        for assertion in &case.assertions {
            let result = evaluator.evaluate(&trajectory, assertion).await;

            eval_results.push_str(&format!("## {}\n", assertion));
            eval_results.push_str(&format!("- Passed: {}\n", result.passed));
            eval_results.push_str(&format!("- Reasoning: {}\n\n", result.reasoning));

            println!("  Assertion: {}", assertion);
            println!("    Passed: {}", result.passed);
            println!("    Reasoning: {}", result.reasoning);

            if !result.passed {
                all_passed = false;
            }
        }

        std::fs::write(case_dir.join("evaluation.md"), &eval_results)
            .expect("failed to write evaluation");

        assert!(
            all_passed,
            "One or more assertions failed for case '{}'. See {}",
            case.name,
            case_dir.display()
        );
    }

    println!("\n=== All cases passed. Artifacts: {} ===", run_dir.display());
}

/// Create the artifact directory for this test run.
/// Returns (dir_path, timestamp_string).
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
        .join("test_results")
        .join(dir_name);
    std::fs::create_dir_all(&dir).expect("failed to create test_results dir");
    (dir, timestamp)
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
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
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
