use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Create a per-test artifact directory under test_results/integration/
pub fn artifact_dir(test_name: &str) -> PathBuf {
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

    let dir_name = format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}-integration",
        year, month, day, hours, minutes, seconds
    );

    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("results")
        .join(dir_name)
        .join(test_name);

    fs::create_dir_all(&dir).expect("failed to create artifact dir");
    dir
}

/// Write a markdown narration of a JSONL session file.
/// Mirrors the system test narrate_session function.
pub fn narrate_session(session_path: &Path) -> String {
    let content = match fs::read_to_string(session_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut out = String::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let role = entry["message"]
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");

        match role {
            "system" => {}
            "user" => {
                let text = entry["message"]
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                out.push_str("---\n\n");
                out.push_str("## User Message\n\n");
                out.push_str(text);
                out.push_str("\n\n");
            }
            "assistant" => {
                out.push_str("---\n\n");
                out.push_str("## Assistant Message\n\n");

                let content_text = entry["message"]
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if !content_text.is_empty() {
                    out.push_str(content_text);
                    out.push_str("\n\n");
                }

                if let Some(reasoning) = entry["message"]
                    .get("reasoning_content")
                    .and_then(|r| r.as_str())
                {
                    if !reasoning.is_empty() {
                        out.push_str("**Reasoning:**\n\n");
                        out.push_str(reasoning);
                        out.push_str("\n\n");
                    }
                }

                if let Some(calls) = entry["message"]
                    .get("tool_calls")
                    .and_then(|t| t.as_array())
                {
                    out.push_str("**Tool Calls:**\n\n");
                    for tc in calls {
                        let name = tc["function"]["name"].as_str().unwrap_or("?");
                        let args = tc["function"]["arguments"].as_str().unwrap_or("");
                        out.push_str(&format!("- `{}({})`\n", name, args));
                    }
                    out.push('\n');
                }
            }
            "tool" => {
                let tool_call_id = entry["message"]
                    .get("tool_call_id")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?");
                let content_text = entry["message"]
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");

                out.push_str("---\n\n");
                out.push_str(&format!("## Tool Result ({})\n\n", tool_call_id));
                out.push_str(content_text);
                out.push_str("\n\n");
            }
            _ => {}
        }
    }

    out
}

/// Write the narration to a .md file next to the session JSONL.
pub fn write_narration(session_path: &Path, narration: &str) {
    let md_path = session_path.with_extension("md");
    fs::write(&md_path, narration).expect("failed to write narration");
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
    let mut month = 1;
    for days_in_month in month_days {
        if days < days_in_month {
            break;
        }
        days -= days_in_month;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}
