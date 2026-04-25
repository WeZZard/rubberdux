use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rubberdux::channel::interpreter::InterpretedMessage;
use rubberdux::channel::{AgentResponse, ChannelEvent};
use rubberdux::hardened_prompts;
use rubberdux::provider::moonshot::MoonshotClient;
use tokio::sync::mpsc;

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
            rubberdux::agent::runtime::chat::run_with_session(rx, client, system_prompt, sp).await;
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
    pub async fn send_message(&self, text: &str, timeout: Duration) -> Vec<AgentResponse> {
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

    /// Send multiple user messages as a batch.  All but the last message are
    /// injected as `ContextUpdate` (added to history without triggering LLM
    /// processing).  The last message is sent as a normal `UserInput` which
    /// triggers the LLM response.
    pub async fn send_messages_batch(
        &self,
        messages: &[String],
        timeout: Duration,
    ) -> Vec<AgentResponse> {
        assert!(
            !messages.is_empty(),
            "batch must contain at least one message"
        );

        // Send all but the last as context updates.
        for text in &messages[..messages.len() - 1] {
            let event = ChannelEvent::ContextUpdate { text: text.clone() };
            self.tx.send(event).await.expect("channel should be open");
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
