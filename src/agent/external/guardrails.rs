use std::sync::Arc;

use crate::guardrail::{PostGuardrail, PostGuardrailAction, PreGuardrail, PreGuardrailResult};

use super::interaction_queue::InteractionQueue;

pub struct InteractionActionGuardrail {
    queue: Arc<InteractionQueue>,
}

impl InteractionActionGuardrail {
    pub fn new(queue: Arc<InteractionQueue>) -> Self {
        Self { queue }
    }
}

impl PostGuardrail for InteractionActionGuardrail {
    fn name(&self) -> &str {
        "interaction_action_required"
    }

    fn check(&self, content: &str) -> PostGuardrailAction {
        // Only check when there are pending interactions
        if self.queue.pending_count() == 0 {
            return PostGuardrailAction::Passed;
        }

        // Check if the response contains actionable content:
        // - tool call for interaction_respond
        // - tool call for agent (spawning subagent)
        // - telegram-message tag (escalating to human)
        let has_tool_call = content.contains("interaction_respond")
            || content.contains("\"name\":\"agent\"")
            || content.contains("\"name\":\"interaction_respond\"");
        let has_escalation = content.contains("<telegram-message");

        if has_tool_call || has_escalation {
            PostGuardrailAction::Passed
        } else {
            // LLM generated text without taking action on the pending interaction
            PostGuardrailAction::NeedsRetry
        }
    }
}

pub struct SafetyGateGuardrail;

const DANGEROUS_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "DROP TABLE",
    "DROP DATABASE",
    "DELETE FROM",
    "TRUNCATE",
    "format c:",
    "mkfs",
    "dd if=",
    "dd of=/dev/",
    ":(){ :|:& };:",
    "shutdown",
    "reboot",
    "passwd",
    "/etc/shadow",
    "creditcard",
    "credit_card",
    "payment",
    "bank_account",
    "private_key",
    "secret_key",
    "api_key",
    "password",
];

impl PreGuardrail for SafetyGateGuardrail {
    fn name(&self) -> &str {
        "safety_gate"
    }

    fn check(&self, content: &str) -> PreGuardrailResult {
        let lower = content.to_lowercase();
        for pattern in DANGEROUS_PATTERNS {
            if lower.contains(&pattern.to_lowercase()) {
                return PreGuardrailResult::Error(format!(
                    "Dangerous operation detected: '{}'. Escalating to human.",
                    pattern
                ));
            }
        }
        PreGuardrailResult::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Post-guardrail tests
    #[test]
    fn test_post_text_only_needs_retry() {
        let queue = Arc::new(InteractionQueue::new());
        // Add a pending interaction so the guardrail fires
        let (tx, _rx) = tokio::sync::oneshot::channel();
        queue.add(
            "q-1".into(),
            super::super::interaction_queue::PendingInteraction {
                request: super::super::UIInteractionRequest::Question {
                    request_id: "q-1".into(),
                    agent_task_id: "".into(),
                    text: "".into(),
                    options: vec![],
                },
                response_tx: tx,
            },
        );

        let guardrail = InteractionActionGuardrail::new(queue);
        // Text without tool calls or telegram-message
        let result =
            guardrail.check("I see the agent is asking about databases. Let me think about this.");
        assert!(matches!(result, PostGuardrailAction::NeedsRetry));
    }

    #[test]
    fn test_post_with_tool_call_passes() {
        let queue = Arc::new(InteractionQueue::new());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        queue.add(
            "q-1".into(),
            super::super::interaction_queue::PendingInteraction {
                request: super::super::UIInteractionRequest::Question {
                    request_id: "q-1".into(),
                    agent_task_id: "".into(),
                    text: "".into(),
                    options: vec![],
                },
                response_tx: tx,
            },
        );

        let guardrail = InteractionActionGuardrail::new(queue);
        let result = guardrail.check(r#"I found the answer. {"name":"interaction_respond"}"#);
        assert!(matches!(result, PostGuardrailAction::Passed));
    }

    #[test]
    fn test_post_no_interaction_passes() {
        let queue = Arc::new(InteractionQueue::new());
        // No pending interactions
        let guardrail = InteractionActionGuardrail::new(queue);
        let result = guardrail.check("Just a normal response about something.");
        assert!(matches!(result, PostGuardrailAction::Passed));
    }

    // Pre-guardrail tests
    #[test]
    fn test_pre_dangerous_operation_errors() {
        let guardrail = SafetyGateGuardrail;
        let result = guardrail.check("Execute: rm -rf / to clean up");
        assert!(matches!(result, PreGuardrailResult::Error(_)));
    }

    #[test]
    fn test_pre_drop_table_errors() {
        let guardrail = SafetyGateGuardrail;
        let result = guardrail.check("Run SQL: DROP TABLE users");
        assert!(matches!(result, PreGuardrailResult::Error(_)));
    }

    #[test]
    fn test_pre_safe_operation_passes() {
        let guardrail = SafetyGateGuardrail;
        let result = guardrail.check("Execute: cargo test --lib");
        assert!(matches!(result, PreGuardrailResult::Passed));
    }

    #[test]
    fn test_pre_case_insensitive() {
        let guardrail = SafetyGateGuardrail;
        let result = guardrail.check("drop table Users");
        assert!(matches!(result, PreGuardrailResult::Error(_)));
    }
}
