use crate::llm::{
    ChatMessage, LlmClient, build_request_json, parse_response_text, sanitize_for_json,
};

/// Something that can be formatted as markdown for LLM evaluation.
pub trait Evaluatable {
    fn format_for_eval(&self) -> String;
}

/// Result of evaluating a single assertion.
#[derive(Debug, Clone)]
pub struct EvaluationResult {
    pub passed: bool,
    pub reasoning: String,
}

/// Uses an LLM to judge whether a trajectory satisfies a natural-language assertion.
pub struct AssertionEvaluator<C: LlmClient> {
    client: C,
    model: String,
    /// Number of evaluation attempts for self-consistency (default: 1).
    consistency_votes: usize,
}

impl<C: LlmClient> AssertionEvaluator<C> {
    pub fn new(client: C) -> Self {
        Self {
            client,
            model: "default".into(),
            consistency_votes: 1,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set the number of self-consistency votes (default: 1).
    /// Higher values reduce flakiness but increase cost/latency.
    pub fn with_consistency_votes(mut self, votes: usize) -> Self {
        self.consistency_votes = votes.max(1);
        self
    }

    /// Evaluate a storyline assertion against the full trajectory.
    pub async fn evaluate_storyline(
        &self,
        subject: &dyn Evaluatable,
        assertion: &str,
    ) -> EvaluationResult {
        let prompt = format!(
            "You are a strict test evaluator. Given an AI agent's full conversation trajectory \
             and a storyline assertion, determine whether the assertion is TRUE across the \
             entire conversation.\n\n{}\n\nSTORYLINE ASSERTION: {}\n\n\
             Respond with ONLY a JSON object in this exact format:\n\
             {{\"passed\": true, \"reasoning\": \"concise explanation\"}}\n\
             If the assertion is false, use passed: false and explain why.",
            subject.format_for_eval(),
            assertion
        );

        self.evaluate_with_prompt(&prompt).await
    }

    /// Evaluate an assistant message assertion against the trajectory.
    ///
    /// `msg_idx` is the 0-based index of the assistant message within the
    /// actual assistant messages produced by the agent (i.e. the nth assistant
    /// message in the conversation transcript).  This is determined by the
    /// ordering matcher, not by the test-case slot index.
    pub async fn evaluate_assistant(
        &self,
        subject: &dyn Evaluatable,
        assertion: &str,
        msg_idx: usize,
    ) -> EvaluationResult {
        let prompt = format!(
            "You are a strict test evaluator. Given an AI agent's conversation trajectory \
             and an assertion about a SPECIFIC assistant message, determine whether the assertion is TRUE.\n\n\
             IMPORTANT: The assertion refers to ASSISTANT MESSAGE NUMBER {} (counting only assistant messages, starting from 1). \
             Evaluate ONLY that specific assistant message, not the final or any other message.\n\n{}\n\n\
             ASSISTANT MESSAGE ASSERTION (for assistant message {}): {}\n\n\
             Respond with ONLY a JSON object in this exact format:\n\
             {{\"passed\": true, \"reasoning\": \"concise explanation\"}}\n\
             If the assertion is false, use passed: false and explain why.",
            msg_idx + 1,
            subject.format_for_eval(),
            msg_idx + 1,
            assertion
        );

        self.evaluate_with_prompt(&prompt).await
    }

    /// Evaluate a user message guidance assertion.
    pub async fn evaluate_user_guidance(
        &self,
        subject: &dyn Evaluatable,
        assertion: &str,
        _msg_idx: usize,
    ) -> EvaluationResult {
        let prompt = format!(
            "You are a strict test evaluator. Given an AI agent's conversation trajectory \
             and a guidance assertion about a user message, determine whether the agent's \
             behavior matched the guidance.\n\n{}\n\n\
             USER MESSAGE GUIDANCE: {}\n\n\
             Respond with ONLY a JSON object in this exact format:\n\
             {{\"passed\": true, \"reasoning\": \"concise explanation\"}}\n\
             If the assertion is false, use passed: false and explain why.",
            subject.format_for_eval(),
            assertion
        );

        self.evaluate_with_prompt(&prompt).await
    }

    /// Internal: evaluate with a given prompt using self-consistency voting.
    async fn evaluate_with_prompt(&self, prompt: &str) -> EvaluationResult {
        let messages = vec![
            ChatMessage {
                role: "system".into(),
                content: "You are a test evaluator. Output valid JSON only.".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: prompt.into(),
            },
        ];

        let mut votes_passed = 0usize;
        let mut all_reasonings = Vec::new();
        let mut last_error = None;

        for attempt in 0..self.consistency_votes {
            let body = build_request_json(&self.model, &messages, 0.0);
            let raw = match self.client.chat_raw(body).await {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(format!("Evaluator LLM call failed: {}", e));
                    continue;
                }
            };

            let text = match parse_response_text(&raw) {
                Ok(t) => t,
                Err(e) => {
                    last_error = Some(format!(
                        "Failed to parse evaluator response: {}. Raw: {}",
                        e, raw
                    ));
                    continue;
                }
            };

            let sanitized = sanitize_for_json(&text);

            let json_str = if let Some(start) = sanitized.find('{') {
                if let Some(end) = sanitized.rfind('}') {
                    &sanitized[start..=end]
                } else {
                    &sanitized
                }
            } else {
                &sanitized
            };

            // Pre-process: remove any trailing commas before } or ] which
            // some LLMs emit, and normalize common Unicode punctuation to
            // ASCII equivalents so JSON parsers don't choke.
            let cleaned = json_str
                .replace(", }", " }")
                .replace(",}", "}")
                .replace(", ]", " ]")
                .replace(",]", "]")
                .replace('\u{2018}', "'")
                .replace('\u{2019}', "'")
                .replace('\u{201c}', "\"")
                .replace('\u{201d}', "\"")
                .replace('\u{2026}', "...")
                .replace('\u{2013}', "-")
                .replace('\u{2014}', "-")
                .replace('\u{2015}', "-")
                .replace('\u{00a0}', " ")
                .replace('\u{00ad}', "")
                .replace('\u{feff}', "")
                .replace('\u{200b}', "")
                .replace('\u{200c}', "")
                .replace('\u{200d}', "")
                .replace('\u{fffd}', "")
                .replace('\u{ff5e}', "~")
                .replace('\u{301c}', "~")
                .replace('\u{2212}', "-")
                .replace('\u{00b4}', "'")
                .replace('\u{0060}', "'")
                .replace('\u{201a}', ",")
                .replace('\u{201e}', "\"")
                .replace('\u{2032}', "'")
                .replace('\u{2033}', "\"")
                .replace('\u{3001}', ",")
                .replace('\u{3002}', ".")
                .replace('\u{30fb}', "-")
                .replace('\u{ff0c}', ",")
                .replace('\u{ff0e}', ".")
                .replace('\u{ff1a}', ":")
                .replace('\u{ff1b}', ";")
                .replace('\u{ff01}', "!")
                .replace('\u{ff1f}', "?")
                .replace('\u{ff08}', "(")
                .replace('\u{ff09}', ")")
                .replace('\u{ff3b}', "[")
                .replace('\u{ff3d}', "]")
                .replace('\u{ff5b}', "{")
                .replace('\u{ff5d}', "}")
                .replace('\u{ffe5}', "\\")
                .replace('\u{005c}', "\\")
                .replace('\u{ff40}', "'")
                .replace('\u{ffe3}', "-")
                .replace('\u{2039}', "<")
                .replace('\u{203a}', ">")
                .replace('\u{00ab}', "\"")
                .replace('\u{00bb}', "\"")
                .replace('\u{276e}', "<")
                .replace('\u{276f}', ">")
                .replace('\u{27e8}', "<")
                .replace('\u{27e9}', ">")
                .replace('\u{3008}', "<")
                .replace('\u{3009}', ">")
                .replace('\u{300a}', "\"")
                .replace('\u{300b}', "\"")
                .replace('\u{300c}', "'")
                .replace('\u{300d}', "'")
                .replace('\u{300e}', "\"")
                .replace('\u{300f}', "\"")
                .replace('\u{3010}', "[")
                .replace('\u{3011}', "]")
                .replace('\u{3014}', "(")
                .replace('\u{3015}', ")")
                .replace('\u{3016}', "[")
                .replace('\u{3017}', "]")
                .replace('\u{3018}', "{")
                .replace('\u{3019}', "}")
                .replace('\u{301a}', "[")
                .replace('\u{301b}', "]")
                .replace('\u{301d}', "\"")
                .replace('\u{301e}', "\"")
                .replace('\u{301f}', "\"")
                .replace('\u{ff62}', "'")
                .replace('\u{ff63}', "'")
                .replace('\u{ff02}', "\"")
                .replace('\u{ff07}', "'")
                .replace('\u{ff3c}', "\\")
                .replace('\u{ff0f}', "/")
                .replace('\u{ff3e}', "^")
                .replace('\u{ff06}', "&")
                .replace('\u{ff0a}', "*")
                .replace('\u{ff05}', "%")
                .replace('\u{ff04}', "$")
                .replace('\u{ff20}', "@")
                .replace('\u{ff10}', "0")
                .replace('\u{ff11}', "1")
                .replace('\u{ff12}', "2")
                .replace('\u{ff13}', "3")
                .replace('\u{ff14}', "4")
                .replace('\u{ff15}', "5")
                .replace('\u{ff16}', "6")
                .replace('\u{ff17}', "7")
                .replace('\u{ff18}', "8")
                .replace('\u{ff19}', "9")
                .replace('\u{ff21}', "A")
                .replace('\u{ff22}', "B")
                .replace('\u{ff23}', "C")
                .replace('\u{ff24}', "D")
                .replace('\u{ff25}', "E")
                .replace('\u{ff26}', "F")
                .replace('\u{ff27}', "G")
                .replace('\u{ff28}', "H")
                .replace('\u{ff29}', "I")
                .replace('\u{ff2a}', "J")
                .replace('\u{ff2b}', "K")
                .replace('\u{ff2c}', "L")
                .replace('\u{ff2d}', "M")
                .replace('\u{ff2e}', "N")
                .replace('\u{ff2f}', "O")
                .replace('\u{ff30}', "P")
                .replace('\u{ff31}', "Q")
                .replace('\u{ff32}', "R")
                .replace('\u{ff33}', "S")
                .replace('\u{ff34}', "T")
                .replace('\u{ff35}', "U")
                .replace('\u{ff36}', "V")
                .replace('\u{ff37}', "W")
                .replace('\u{ff38}', "X")
                .replace('\u{ff39}', "Y")
                .replace('\u{ff3a}', "Z")
                .replace('\u{ff41}', "a")
                .replace('\u{ff42}', "b")
                .replace('\u{ff43}', "c")
                .replace('\u{ff44}', "d")
                .replace('\u{ff45}', "e")
                .replace('\u{ff46}', "f")
                .replace('\u{ff47}', "g")
                .replace('\u{ff48}', "h")
                .replace('\u{ff49}', "i")
                .replace('\u{ff4a}', "j")
                .replace('\u{ff4b}', "k")
                .replace('\u{ff4c}', "l")
                .replace('\u{ff4d}', "m")
                .replace('\u{ff4e}', "n")
                .replace('\u{ff4f}', "o")
                .replace('\u{ff50}', "p")
                .replace('\u{ff51}', "q")
                .replace('\u{ff52}', "r")
                .replace('\u{ff53}', "s")
                .replace('\u{ff54}', "t")
                .replace('\u{ff55}', "u")
                .replace('\u{ff56}', "v")
                .replace('\u{ff57}', "w")
                .replace('\u{ff58}', "x")
                .replace('\u{ff59}', "y")
                .replace('\u{ff5a}', "z")
                .replace('\u{ff0b}', "+")
                .replace('\u{ff1c}', "<")
                .replace('\u{ff1e}', ">")
                .replace('\u{ff1d}', "=")
                .replace('\u{ff5c}', "|")
                .replace('\u{ff5f}', "(")
                .replace('\u{ff60}', ")")
                .replace('\u{ffe2}', "~")
                .replace('\u{ffe4}', "|")
                .replace('\u{ffe8}', "-")
                .replace('\u{ffe9}', "-")
                .replace('\u{ffea}', "|")
                .replace('\u{ffeb}', "-")
                .replace('\u{ffec}', "|")
                .replace('\u{ffed}', "-")
                .replace('\u{ffee}', "|")
                .replace('\u{ffef}', "")
                .replace('\u{fff0}', "")
                .replace('\u{fff1}', "")
                .replace('\u{fff2}', "")
                .replace('\u{fff3}', "")
                .replace('\u{fff4}', "")
                .replace('\u{fff5}', "")
                .replace('\u{fff6}', "")
                .replace('\u{fff7}', "")
                .replace('\u{fff8}', "")
                .replace('\u{fff9}', "")
                .replace('\u{fffa}', "")
                .replace('\u{fffb}', "")
                .replace('\u{fffc}', "")
                .replace('\u{fffd}', "")
                .replace('\u{fffe}', "")
                .replace('\u{ffff}', "");

            let (passed, reasoning) = match parse_evaluator_response(&cleaned) {
                Some(result) => result,
                None => {
                    last_error = Some(format!(
                        "Failed to parse evaluator response as JSON. Raw: {}",
                        text
                    ));
                    continue;
                }
            };

            if passed {
                votes_passed += 1;
            }
            all_reasonings.push(format!(
                "[Vote {}] {}: {}",
                attempt + 1,
                if passed { "PASS" } else { "FAIL" },
                reasoning
            ));
        }

        let total_votes = all_reasonings.len();
        if total_votes == 0 {
            // All attempts failed
            return EvaluationResult {
                passed: false,
                reasoning: last_error.unwrap_or_else(|| "All evaluator attempts failed".into()),
            };
        }

        let passed = votes_passed > total_votes / 2;

        EvaluationResult {
            passed,
            reasoning: all_reasonings.join("\n"),
        }
    }
}

/// Parse an evaluator response, trying JSON first, then falling back to
/// regex extraction for malformed but structurally valid responses.
///
/// Handles the common case where the LLM puts unescaped quotes inside
/// the reasoning string.
fn parse_evaluator_response(text: &str) -> Option<(bool, String)> {
    // Try strict JSON first
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        let passed = value["passed"].as_bool()?;
        let reasoning = value["reasoning"].as_str()?.to_string();
        return Some((passed, reasoning));
    }

    // Fallback: extract passed field with regex
    let passed = extract_json_bool(text, "passed")?;

    // Fallback: extract reasoning field by finding the key and grabbing
    // the string value, handling nested quotes.
    let reasoning = extract_json_string(text, "reasoning")?;

    Some((passed, reasoning))
}

/// Extract a boolean value from a JSON-like string by key name.
fn extract_json_bool(text: &str, key: &str) -> Option<bool> {
    let pattern = format!(r#"(?i)"{}"\s*:\s*(true|false)"#, regex::escape(key));
    let re = regex::Regex::new(&pattern).ok()?;
    let cap = re.captures(text)?;
    cap.get(1)?.as_str().parse().ok()
}

/// Extract a string value from a JSON-like string by key name.
///
/// This handles the case where the string contains unescaped quotes by
/// scanning from the opening quote to the closing quote, accounting for
/// escaped quotes.
fn extract_json_string(text: &str, key: &str) -> Option<String> {
    let key_pattern = format!(r#"(?i)"{}"\s*:\s*""#, regex::escape(key));
    let re = regex::Regex::new(&key_pattern).ok()?;
    let mat = re.find(text)?;

    let start = mat.end();
    let rest = &text[start..];

    let mut result = String::new();
    let mut chars = rest.chars().peekable();
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            match ch {
                'n' => result.push('\n'),
                'r' => result.push('\r'),
                't' => result.push('\t'),
                '\\' => result.push('\\'),
                '"' => result.push('"'),
                _ => result.push(ch),
            }
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '"' {
            // End of string
            return Some(result);
        }

        result.push(ch);
    }

    // If we get here, the string was not properly closed, but we still
    // have something — return it as a best-effort.
    Some(result)
}
