use std::fs;
use std::path::{Path, PathBuf};

/// Create a per-test artifact directory under the cached integration run root.
pub fn artifact_dir(test_name: &str) -> PathBuf {
    super::artifacts::integration_case_dir(test_name)
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
