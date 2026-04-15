use rubberdux::provider::moonshot::{Message, MoonshotClient, UserContent};

use super::harness::Trajectory;

/// Result of evaluating a single assertion.
pub struct EvaluationResult {
    pub passed: bool,
    pub reasoning: String,
}

/// Uses a real LLM to judge whether a trajectory satisfies a natural-language assertion.
pub struct AssertionEvaluator {
    client: MoonshotClient,
}

impl AssertionEvaluator {
    pub fn from_env() -> Self {
        Self {
            client: MoonshotClient::from_env(),
        }
    }

    /// Ask the evaluator model whether the assertion is true for the given trajectory.
    pub async fn evaluate(&self, trajectory: &Trajectory, assertion: &str) -> EvaluationResult {
        let prompt = format!(
            "You are a strict test evaluator. Given an AI agent trajectory and a natural-language assertion, determine whether the assertion is TRUE.\n\n{}\n\nASSERTION: {}\n\nRespond with ONLY a JSON object in this exact format:\n{{\"passed\": true, \"reasoning\": \"concise explanation\"}}\nIf the assertion is false, use passed: false and explain why.",
            trajectory.format_for_eval(),
            assertion
        );

        let messages = vec![
            Message::System {
                content: "You are a test evaluator. Output valid JSON only.".into(),
            },
            Message::User {
                content: UserContent::Text(prompt),
            },
        ];

        let response = match self.client.chat(messages, None).await {
            Ok(r) => r,
            Err(e) => {
                return EvaluationResult {
                    passed: false,
                    reasoning: format!("Evaluator LLM call failed: {}", e),
                };
            }
        };

        let text = response.choices[0].message.content_text();

        let json_str = if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                &text[start..=end]
            } else {
                text
            }
        } else {
            text
        };

        let value: serde_json::Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                return EvaluationResult {
                    passed: false,
                    reasoning: format!(
                        "Failed to parse evaluator response as JSON: {}. Raw: {}",
                        e, text
                    ),
                };
            }
        };

        EvaluationResult {
            passed: value["passed"].as_bool().unwrap_or(false),
            reasoning: value["reasoning"]
                .as_str()
                .unwrap_or("no reasoning provided")
                .to_string(),
        }
    }
}
