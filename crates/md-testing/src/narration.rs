use std::path::Path;

/// Maximum length for tool result content before truncation.
const TOOL_RESULT_MAX_LEN: usize = 2000;
/// Maximum number of lines to show from tool results before truncation.
const TOOL_RESULT_MAX_LINES: usize = 40;

/// Convert a session JSONL file into human-readable markdown sections.
pub fn narrate_session(session_path: &Path, out: &mut String) {
    let content = match std::fs::read_to_string(session_path) {
        Ok(c) => c,
        Err(_) => return,
    };

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
                out.push('\n');
                out.push('\n');
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
                    out.push('\n');
                    out.push('\n');
                }

                if let Some(reasoning) = entry["message"]
                    .get("reasoning_content")
                    .and_then(|r| r.as_str())
                {
                    if !reasoning.is_empty() {
                        out.push_str("**Reasoning:**\n\n");
                        out.push_str(reasoning);
                        out.push('\n');
                        out.push('\n');
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

                // Summarize large tool results to avoid overwhelming the evaluator
                let summarized = summarize_tool_result(content_text);
                out.push_str(&summarized);
                out.push('\n');
                out.push('\n');
            }
            _ => {}
        }
    }
}

/// Summarize a tool result to keep evaluator context manageable.
///
/// - If the content is short, return it as-is.
/// - If it's long, truncate to first N lines and add a summary note.
/// - For known large-output tools (system_profiler, find, glob, etc.),
///   provide a structured summary.
fn summarize_tool_result(content: &str) -> String {
    let line_count = content.lines().count();
    let char_count = content.len();

    // If it's already short, return as-is
    if char_count <= TOOL_RESULT_MAX_LEN && line_count <= TOOL_RESULT_MAX_LINES {
        return content.to_string();
    }

    // Try to provide a meaningful summary based on content type
    if content.contains("SPHardwareDataType") || content.contains("system_profiler") {
        return summarize_system_profiler(content);
    }

    if content.contains("<?xml") || content.contains("<!DOCTYPE plist") {
        return summarize_xml(content);
    }

    // Generic truncation with preview
    let mut result = String::new();
    result.push_str(&format!(
        "*(Tool output truncated: {} lines, {} chars total)*\n\n",
        line_count, char_count
    ));

    result.push_str("**Preview (first 40 lines):**\n\n");
    result.push_str("```\n");
    for (_i, line) in content.lines().take(TOOL_RESULT_MAX_LINES).enumerate() {
        result.push_str(line);
        result.push('\n');
        // Also enforce char limit within the preview
        if result.len() > TOOL_RESULT_MAX_LEN {
            result.push_str("...\n");
            break;
        }
    }
    result.push_str("```\n");

    // Add a tail preview for very long outputs
    if line_count > TOOL_RESULT_MAX_LINES * 2 {
        result.push_str("\n**Tail (last 5 lines):**\n\n");
        result.push_str("```\n");
        for line in content.lines().skip(line_count.saturating_sub(5)) {
            result.push_str(line);
            result.push('\n');
        }
        result.push_str("```\n");
    }

    result
}

/// Extract key information from system_profiler output.
fn summarize_system_profiler(content: &str) -> String {
    let mut hardware_info = Vec::new();
    let mut in_hardware = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "Hardware:" || trimmed.contains("SPHardwareDataType") {
            in_hardware = true;
        }
        if in_hardware {
            if trimmed.starts_with("Model Name:")
                || trimmed.starts_with("Model Identifier:")
                || trimmed.starts_with("Chip:")
                || trimmed.starts_with("Total Number of Cores:")
                || trimmed.starts_with("Memory:")
            {
                hardware_info.push(trimmed.to_string());
            }
            // Stop after we've collected the key fields
            if trimmed.is_empty() && !hardware_info.is_empty() {
                break;
            }
        }
    }

    let mut result = String::new();
    result.push_str("*(system_profiler output summarized)*\n\n");
    result.push_str("**Key Hardware Info:**\n");
    for info in hardware_info {
        result.push_str(&format!("- {}\n", info));
    }
    result
}

/// Summarize XML/plist output.
fn summarize_xml(content: &str) -> String {
    let line_count = content.lines().count();
    let mut result = String::new();
    result.push_str(&format!(
        "*(XML/plist output truncated: {} lines, {} chars)*\n\n",
        line_count,
        content.len()
    ));
    result.push_str("**First 20 lines:**\n\n");
    result.push_str("```xml\n");
    for line in content.lines().take(20) {
        result.push_str(line);
        result.push('\n');
    }
    result.push_str("...\n```\n");
    result
}

/// Write narration files for each subagent session found next to `session_path`.
pub fn write_subagent_narrations(session_path: &Path, case_name: &str, test_time: &str) {
    let subagents_dir = session_path.parent().unwrap().join("subagents");
    if !subagents_dir.is_dir() {
        return;
    }

    let Ok(entries) = std::fs::read_dir(&subagents_dir) else {
        return;
    };

    let mut jsonl_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    jsonl_files.sort_by_key(|e| e.path());

    for entry in jsonl_files {
        let jsonl_path = entry.path();
        let agent_id = jsonl_path
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let mut s = String::new();
        s.push_str("---\n");
        s.push_str(&format!("agent_id: {}\n", agent_id));
        s.push_str(&format!("parent_testcase: {}\n", case_name));
        s.push_str(&format!("test_time: {}\n", test_time));
        s.push_str("---\n\n");
        s.push_str(&format!("# Subagent {}\n\n", agent_id));

        // Include metadata if present
        let meta_path = subagents_dir.join(format!("{}.meta.json", agent_id));
        if let Ok(meta_text) = std::fs::read_to_string(&meta_path) {
            s.push_str(&format!("**Metadata:** {}\n\n", meta_text.trim()));
        }

        // Narrate the subagent session
        narrate_session(&jsonl_path, &mut s);

        let narration_path = subagents_dir.join(format!("{}.md", agent_id));
        let _ = std::fs::write(&narration_path, &s);
    }
}

/// Convert a snake_case name to Title Case.
pub fn humanize_case_name(name: &str) -> String {
    name.replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
