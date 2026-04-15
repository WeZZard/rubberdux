use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rubberdux::channel::{AgentResponse, ChannelEvent};
use rubberdux::channel::interpreter::InterpretedMessage;
use rubberdux::hardened_prompts;
use rubberdux::provider::moonshot::MoonshotClient;
use tokio::sync::mpsc;

/// Complete record of a channel-level agent run.
pub struct Trajectory {
    pub case_name: String,
    pub test_time: String,
    pub user_message: String,
    pub responses: Vec<AgentResponse>,
    pub session_path: PathBuf,
}

impl Trajectory {
    /// Format main agent narration as structured markdown.
    pub fn format_for_eval(&self) -> String {
        let mut s = String::new();

        // Frontmatter
        s.push_str("---\n");
        s.push_str(&format!("testcase_name: {}\n", self.case_name));
        s.push_str(&format!("test_time: {}\n", self.test_time));
        s.push_str("---\n\n");

        // Title
        let title = humanize_case_name(&self.case_name);
        s.push_str(&format!("# Test Case: {}\n\n", title));

        // Session entries
        narrate_session(&self.session_path, &mut s);

        // Channel delivery summary
        s.push_str("---\n\n");
        s.push_str("## Channel Delivery\n\n");
        s.push_str("Messages delivered to Telegram:\n\n");
        for (i, response) in self.responses.iter().enumerate() {
            s.push_str(&format!(
                "{}. `is_final={}` `entry_id={}` `reply_to={:?}`\n\n",
                i + 1,
                response.is_final,
                response.entry_id,
                response.reply_to_message_id,
            ));
            s.push_str(&response.text);
            s.push_str("\n\n");
        }

        s
    }

    /// Write narration files for each subagent session.
    pub fn write_subagent_narrations(&self) {
        let subagents_dir = self.session_path.parent().unwrap().join("subagents");
        if !subagents_dir.is_dir() {
            return;
        }

        let Ok(entries) = std::fs::read_dir(&subagents_dir) else {
            return;
        };

        let mut jsonl_files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|x| x == "jsonl")
                    .unwrap_or(false)
            })
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
            s.push_str(&format!("parent_testcase: {}\n", self.case_name));
            s.push_str(&format!("test_time: {}\n", self.test_time));
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
}

/// Convert a session JSONL file into human-readable markdown sections.
fn narrate_session(session_path: &Path, out: &mut String) {
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
}

fn humanize_case_name(name: &str) -> String {
    name.strip_prefix("testcase_")
        .unwrap_or(name)
        .replace('_', " ")
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

/// Test harness that drives `chat::run_with_session()` at the channel boundary.
pub struct ChannelHarness {
    tx: mpsc::Sender<ChannelEvent>,
    session_path: PathBuf,
    _join: tokio::task::JoinHandle<()>,
}

impl ChannelHarness {
    pub async fn new(system_prompt: &str, session_path: PathBuf) -> Self {
        let client = Arc::new(MoonshotClient::from_env());
        let (tx, rx) = mpsc::channel::<ChannelEvent>(32);
        let system_prompt = system_prompt.to_string();
        let sp = session_path.clone();

        let join = tokio::spawn(async move {
            rubberdux::agent::runtime::chat::run_with_session(
                rx, client, system_prompt, sp,
            )
            .await;
        });

        Self {
            tx,
            session_path,
            _join: join,
        }
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// Send a user message and collect all `AgentResponse` messages
    /// until `is_final == true` or the timeout expires.
    pub async fn send_message(
        &self,
        text: &str,
        timeout: Duration,
    ) -> Vec<AgentResponse> {
        let (reply_tx, mut reply_rx) = mpsc::channel::<AgentResponse>(32);

        let interpreted = InterpretedMessage {
            text: text.to_string(),
            attachments: vec![],
        };

        let event = ChannelEvent::UserInput {
            interpreted,
            reply_tx: Some(reply_tx),
            telegram_message_id: Some(1),
        };

        self.tx.send(event).await.expect("channel should be open");

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            match tokio::time::timeout_at(deadline, reply_rx.recv()).await {
                Ok(Some(response)) => {
                    let is_final = response.is_final;
                    responses.push(response);
                    if is_final {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        responses
    }
}

/// Build the system prompt the same way production does.
pub fn build_system_prompt() -> String {
    let prompt_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
    let parts = hardened_prompts::load_prompt_parts(&prompt_dir);
    hardened_prompts::compose_system_prompt(&parts, None)
}
