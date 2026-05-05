use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rubberdux::agent::entry::{Entry, EntryOrigin};
use rubberdux::agent::runtime::port::{EntryNotification, InputPort, LoopEvent};
use rubberdux::hardened_prompts;
use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};

/// A collected response from the agent loop, matching the shape tests expect.
#[derive(Debug, Clone)]
pub struct AgentResponse {
    pub text: String,
    pub entry_id: usize,
    pub is_final: bool,
}

/// Complete record of a channel-level agent run.
pub struct Trajectory {
    pub case_name: String,
    pub test_time: String,
    pub user_messages: Vec<String>,
    pub responses: Vec<AgentResponse>,
    pub session_path: PathBuf,
}

impl md_testing::Evaluatable for Trajectory {
    fn format_for_eval(&self) -> String {
        let mut s = String::new();

        // Frontmatter
        s.push_str("---\n");
        s.push_str(&format!("testcase_name: {}\n", self.case_name));
        s.push_str(&format!("test_time: {}\n", self.test_time));
        s.push_str("---\n\n");

        // Title
        let title = md_testing::narration::humanize_case_name(&self.case_name);
        s.push_str(&format!("# Test Case: {}\n\n", title));

        // User messages
        s.push_str("## User Messages\n\n");
        for (i, msg) in self.user_messages.iter().enumerate() {
            s.push_str(&format!("{}. {}\n\n", i + 1, msg));
        }

        // Session entries
        md_testing::narration::narrate_session(&self.session_path, &mut s);

        // Channel delivery summary
        s.push_str("---\n\n");
        s.push_str("## Channel Delivery\n\n");
        s.push_str("Messages delivered:\n\n");
        for (i, response) in self.responses.iter().enumerate() {
            s.push_str(&format!(
                "{}. `is_final={}` `entry_id={}`\n\n",
                i + 1,
                response.is_final,
                response.entry_id,
            ));
            s.push_str(&response.text);
            s.push('\n');
            s.push('\n');
        }

        s
    }
}

impl Trajectory {
    /// Write narration files for each subagent session.
    pub fn write_subagent_narrations(&self) {
        md_testing::narration::write_subagent_narrations(
            &self.session_path,
            &self.case_name,
            &self.test_time,
        );
    }
}

/// Test harness that drives `chat::run_with_session()` at the InputPort boundary.
pub struct ChannelHarness {
    input_port: InputPort,
    output_rx: tokio::sync::Mutex<tokio::sync::broadcast::Receiver<EntryNotification>>,
    session_path: PathBuf,
    _join: tokio::task::JoinHandle<()>,
}

pub struct MessageExchange {
    pub responses: Vec<AgentResponse>,
    pub failure_reason: Option<String>,
}

impl ChannelHarness {
    pub async fn new(system_prompt: &str, session_path: PathBuf) -> Self {
        let client = Arc::new(MoonshotClient::from_env());
        let system_prompt = system_prompt.to_string();
        let sp = session_path.clone();

        let (agent_loop, input_port) =
            rubberdux::agent::runtime::chat::run_with_session(client, system_prompt, sp).await;

        let output_rx = agent_loop.subscribe_output().into_receiver();

        let join = tokio::spawn(async move {
            agent_loop.run().await;
        });

        Self {
            input_port,
            output_rx: tokio::sync::Mutex::new(output_rx),
            session_path,
            _join: join,
        }
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    /// Send a user message and collect all assistant `EntryNotification` messages
    /// until `is_final == true` or the timeout expires.
    pub async fn send_message(&self, text: &str, timeout: Duration) -> MessageExchange {
        let message = Message::User {
            content: UserContent::Text(text.to_string()),
        };

        let origin = EntryOrigin::User {
            channel: "test".into(),
        };

        if let Err(e) = self.input_port.send_user_message(message, origin).await {
            return MessageExchange {
                responses: vec![],
                failure_reason: Some(format!("Failed to send message: {}", e)),
            };
        }

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut failure_reason = None;
        let mut rx = self.output_rx.lock().await;

        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(notification)) => {
                    // Only collect assistant entries
                    if let Message::Assistant { content, .. } = &notification.entry.message {
                        let text = content.clone().unwrap_or_default();
                        responses.push(AgentResponse {
                            text,
                            entry_id: notification.entry.id,
                            is_final: notification.is_final,
                        });
                        if notification.is_final {
                            break;
                        }
                    }
                }
                Ok(Err(_recv_err)) => {
                    failure_reason =
                        Some("Broadcast channel closed before a final assistant response".into());
                    break;
                }
                Err(_) => {
                    failure_reason = Some(format!(
                        "Timed out after {} second(s) waiting for a final assistant response",
                        timeout.as_secs()
                    ));
                    break;
                }
            }
        }

        MessageExchange {
            responses,
            failure_reason,
        }
    }

    /// Send multiple user messages as a batch. All but the last message are
    /// injected as `ContextUpdate` (added to history without triggering LLM
    /// processing). The last message is sent as a normal user message which
    /// triggers the LLM response.
    pub async fn send_messages_batch(
        &self,
        messages: &[String],
        timeout: Duration,
    ) -> MessageExchange {
        assert!(
            !messages.is_empty(),
            "batch must contain at least one message"
        );

        // Send all but the last as context updates.
        for text in &messages[..messages.len() - 1] {
            let message = Message::User {
                content: UserContent::Text(text.clone()),
            };
            if let Err(e) = self.input_port.send_context_update(message).await {
                return MessageExchange {
                    responses: vec![],
                    failure_reason: Some(format!("Failed to send context update: {}", e)),
                };
            }
        }

        // Send the last message as a normal user input to trigger LLM.
        self.send_message(&messages[messages.len() - 1], timeout)
            .await
    }
}

/// Build the system prompt the same way production does.
pub fn build_system_prompt() -> String {
    let prompt_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
    let parts = hardened_prompts::load_prompt_parts(&prompt_dir);
    hardened_prompts::compose_system_prompt(&parts, None)
}
