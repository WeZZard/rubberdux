//! App identity derivation — title + icon (SF Symbol + color).
//!
//! The public entry point is [`derive_identity`], which is infallible: it
//! always returns a valid [`AppIdentity`]. The LLM response is validated
//! field-by-field; only invalid fields are replaced via the heuristic fallback.
//! If the entire LLM call fails or returns malformed JSON, the full heuristic
//! fallback is used. See `docs/app/identity.md` for the design rationale.

pub mod fallback;
pub mod prompt;

use serde::{Deserialize, Serialize};

use crate::app::IconSpec;
use crate::provider::moonshot::{extract_json_object, MoonshotClient};

use fallback::{fallback_color, fallback_symbol, fallback_title};

// ---------------------------------------------------------------------------
// Allowlist and palette
// ---------------------------------------------------------------------------

/// SF Symbol names that are available on macOS 13+ and suitable as task-domain
/// metaphors. The LLM must pick from this list; any other value is replaced by
/// the heuristic fallback. See `docs/app/identity.md` for rationale.
pub const SYMBOL_ALLOWLIST: &[&str] = &[
    "star",
    "heart",
    "bolt",
    "flame",
    "leaf",
    "globe",
    "house",
    "calendar",
    "clock",
    "briefcase",
    "envelope",
    "doc",
    "folder",
    "magnifyingglass",
    "pencil",
    "wrench",
    "gear",
    "person",
    "person.2",
    "cart",
    "creditcard",
    "camera",
    "music.note",
    "airplane",
];

/// Color palette: ten semantic hex colors for task-domain coloring. The LLM
/// must pick from this list; any other value is replaced by the heuristic
/// fallback.
pub const COLOR_PALETTE: &[&str] = &[
    "#3478F6", // blue — communication
    "#34C759", // green — productivity
    "#FF9500", // orange — creative
    "#FF3B30", // red — urgent / critical
    "#AF52DE", // purple — research
    "#5AC8FA", // light blue — information
    "#FFCC00", // yellow — ideas
    "#FF2D55", // pink — social
    "#4CD964", // mint — wellness
    "#8E8E93", // gray — miscellaneous
];

// ---------------------------------------------------------------------------
// Public output type
// ---------------------------------------------------------------------------

/// The derived identity of an [`crate::app::App`]: a short display title plus
/// an icon (SF Symbol glyph + color). Returned by [`derive_identity`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIdentity {
    /// Short descriptive title derived from the task (max 40 chars).
    pub title: String,
    /// The icon to display on the whiteboard tile.
    pub icon: IconSpec,
}

// ---------------------------------------------------------------------------
// Internal LLM response shape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LlmIdentityResponse {
    title: Option<String>,
    sf_symbol: Option<String>,
    color: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Derive an [`AppIdentity`] from a task string.
///
/// Makes a constrained JSON call to the Moonshot API. Per-field validation
/// repairs only the invalid field; a valid title from the LLM response is
/// always kept even if the symbol or color must be replaced. If the LLM call
/// fails or the response is not valid JSON, the full heuristic fallback is used.
///
/// This function never returns an error.
pub async fn derive_identity(client: &MoonshotClient, task: &str) -> AppIdentity {
    let messages = vec![prompt::system_message(), prompt::user_message(task)];

    // The model is a reasoning model: it spends completion tokens on a visible
    // chain-of-thought before emitting the answer. The budget must leave room for
    // both, or the response is truncated (`finish_reason: "length"`) with empty
    // `content`. The JSON shape is requested at the prompt level (see
    // `prompt::system_message`), which the model honors, so no `response_format`
    // constraint is set. See `docs/app/identity.md`.
    let request = crate::provider::moonshot::api::chat::ChatRequest {
        model: client.model().to_owned(),
        messages,
        temperature: Some(0.3),
        max_completion_tokens: Some(2048),
        tools: None,
        response_format: None,
        thinking: None,
    };

    let raw_json = fetch_raw_json(client, request).await;

    match raw_json {
        Some(json) => repair_from_json(&json, task),
        None => full_fallback(task),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Send the request and extract the text content of the first assistant choice.
/// Returns `None` on any network / HTTP / parse error.
async fn fetch_raw_json(
    client: &MoonshotClient,
    request: crate::provider::moonshot::api::chat::ChatRequest,
) -> Option<String> {
    let response = client
        .http()
        .post(client.url("/chat/completions"))
        .header("Authorization", client.auth_header())
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        log::warn!(
            "identity: LLM call failed with status {}: {}",
            status,
            body
        );
        return None;
    }

    let chat: crate::provider::moonshot::api::chat::ChatResponse =
        response.json().await.ok()?;

    let text = chat.choices.into_iter().next()?.message;
    let content = text.content_text().trim();
    // Empty/whitespace content (e.g. a truncated reasoning response) yields `None`
    // so the deterministic fallback runs cleanly rather than hitting the
    // parse-error path with an empty string.
    if content.is_empty() {
        return None;
    }
    Some(content.to_owned())
}

/// Parse the raw JSON from the LLM and repair any invalid fields, keeping
/// valid ones. Falls back to `full_fallback` if parsing fails entirely.
fn repair_from_json(json: &str, task: &str) -> AppIdentity {
    // Extract the first balanced JSON object, tolerating markdown fences or
    // surrounding prose the reasoning model may emit around the answer.
    let object = match extract_json_object(json) {
        Some(o) => o,
        None => {
            log::warn!("identity: no JSON object in LLM response, using full fallback");
            return full_fallback(task);
        }
    };

    let parsed: Result<LlmIdentityResponse, _> = serde_json::from_str(object);

    let llm = match parsed {
        Ok(r) => r,
        Err(e) => {
            log::warn!("identity: malformed JSON from LLM ({}), using full fallback", e);
            return full_fallback(task);
        }
    };

    // Validate each field independently and repair only the invalid one.
    let title = validate_title(llm.title).unwrap_or_else(|| fallback_title(task));
    let symbol = validate_symbol(llm.sf_symbol).unwrap_or_else(|| fallback_symbol(task).to_owned());
    let color = validate_color(llm.color).unwrap_or_else(|| fallback_color(task).to_owned());

    AppIdentity {
        title,
        icon: IconSpec { symbol, color },
    }
}

/// Full heuristic fallback — used when the LLM call fails or returns
/// unparseable JSON.
fn full_fallback(task: &str) -> AppIdentity {
    AppIdentity {
        title: fallback_title(task),
        icon: IconSpec {
            symbol: fallback_symbol(task).to_owned(),
            color: fallback_color(task).to_owned(),
        },
    }
}

fn validate_title(title: Option<String>) -> Option<String> {
    let t = title?;
    let trimmed = t.trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        // Truncate to 40 chars if the model returned a longer title.
        Some(trimmed.chars().take(40).collect())
    }
}

fn validate_symbol(symbol: Option<String>) -> Option<String> {
    let s = symbol?;
    let trimmed = s.trim();
    if SYMBOL_ALLOWLIST.contains(&trimmed) {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

fn validate_color(color: Option<String>) -> Option<String> {
    let c = color?;
    let trimmed = c.trim();
    if COLOR_PALETTE.contains(&trimmed) {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Unit tests — use a mock HTTP server; no live LLM calls.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_client(base_url: &str) -> MoonshotClient {
        MoonshotClient::new(
            reqwest::Client::new(),
            base_url.to_owned(),
            "test-key".to_owned(),
            "test-model".to_owned(),
        )
    }

    fn chat_response_body(content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "cmpl-test",
            "object": "chat.completion",
            "created": 1234567890u64,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "reasoning_content": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30,
                "cached_tokens": 0
            }
        })
    }

    /// derive_identity is deterministic for the same task when the LLM always
    /// returns the same JSON (mocked).
    #[tokio::test]
    async fn derive_identity_is_deterministic_with_mock() {
        let server = MockServer::start().await;
        let content_json = r##"{"title":"Plan trip","sf_symbol":"airplane","color":"#3478F6"}"##;
        let body = serde_json::to_string(&chat_response_body(content_json)).unwrap();

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .expect(2)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let task = "Book a flight to Tokyo";
        let id1 = derive_identity(&client, task).await;
        let id2 = derive_identity(&client, task).await;
        assert_eq!(id1, id2);
    }

    /// When the LLM returns a valid identity, all three fields are preserved.
    #[tokio::test]
    async fn valid_llm_response_is_preserved() {
        let server = MockServer::start().await;
        let response_json =
            r##"{"title":"Write a report","sf_symbol":"doc","color":"#34C759"}"##;
        let body =
            serde_json::to_string(&chat_response_body(response_json)).unwrap();

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let id = derive_identity(&client, "Write a quarterly report").await;
        assert_eq!(id.title, "Write a report");
        assert_eq!(id.icon.symbol, "doc");
        assert_eq!(id.icon.color, "#34C759");
    }

    /// When the symbol is off-allowlist, only the symbol is repaired; the valid
    /// title and color from the LLM are kept.
    #[tokio::test]
    async fn invalid_symbol_is_repaired_only() {
        let server = MockServer::start().await;
        // "rocket" is not in the allowlist.
        let response_json =
            r##"{"title":"Launch project","sf_symbol":"rocket","color":"#FF9500"}"##;
        let body =
            serde_json::to_string(&chat_response_body(response_json)).unwrap();

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let task = "Launch the new product";
        let id = derive_identity(&client, task).await;

        // Title from LLM is preserved.
        assert_eq!(id.title, "Launch project");
        // Color from LLM is preserved — note: #FF9500 is in COLOR_PALETTE.
        assert_eq!(id.icon.color, "#FF9500");
        // Symbol was invalid — must be in the allowlist now.
        assert!(
            SYMBOL_ALLOWLIST.contains(&id.icon.symbol.as_str()),
            "repaired symbol '{}' not in allowlist",
            id.icon.symbol
        );
    }

    /// When the color is off-palette, only the color is repaired; the valid
    /// title and symbol from the LLM are kept.
    #[tokio::test]
    async fn invalid_color_is_repaired_only() {
        let server = MockServer::start().await;
        // "#ABCDEF" is not in the palette.
        let response_json =
            r##"{"title":"Debug issue","sf_symbol":"wrench","color":"#ABCDEF"}"##;
        let body =
            serde_json::to_string(&chat_response_body(response_json)).unwrap();

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let task = "Debug the failing test";
        let id = derive_identity(&client, task).await;

        assert_eq!(id.title, "Debug issue");
        assert_eq!(id.icon.symbol, "wrench");
        assert!(
            COLOR_PALETTE.contains(&id.icon.color.as_str()),
            "repaired color '{}' not in palette",
            id.icon.color
        );
    }

    /// Malformed JSON triggers the full heuristic fallback; the function still
    /// returns a valid identity.
    #[tokio::test]
    async fn malformed_json_triggers_full_fallback() {
        let server = MockServer::start().await;
        let body =
            serde_json::to_string(&chat_response_body("not json at all {{{}}}")).unwrap();

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let task = "Organize the team standup";
        let id = derive_identity(&client, task).await;

        // Must be valid identity — symbol in allowlist, color in palette.
        assert!(
            SYMBOL_ALLOWLIST.contains(&id.icon.symbol.as_str()),
            "fallback symbol '{}' not in allowlist",
            id.icon.symbol
        );
        assert!(
            COLOR_PALETTE.contains(&id.icon.color.as_str()),
            "fallback color '{}' not in palette",
            id.icon.color
        );
        // Title must be non-empty.
        assert!(!id.title.is_empty());
    }

    /// HTTP failure triggers the full heuristic fallback.
    #[tokio::test]
    async fn http_failure_triggers_full_fallback() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let task = "Review pull request changes";
        let id = derive_identity(&client, task).await;

        assert!(SYMBOL_ALLOWLIST.contains(&id.icon.symbol.as_str()));
        assert!(COLOR_PALETTE.contains(&id.icon.color.as_str()));
        assert!(!id.title.is_empty());
    }

    /// Full fallback is deterministic: same task always yields same identity.
    #[test]
    fn full_fallback_is_deterministic() {
        let task = "Write integration tests for the auth module";
        let id1 = full_fallback(task);
        let id2 = full_fallback(task);
        assert_eq!(id1, id2);
    }

    /// repair_from_json with valid JSON preserves all LLM fields.
    #[test]
    fn repair_from_json_preserves_valid_fields() {
        let json = r##"{"title":"Check logs","sf_symbol":"doc","color":"#3478F6"}"##;
        let id = repair_from_json(json, "Check server logs");
        assert_eq!(id.title, "Check logs");
        assert_eq!(id.icon.symbol, "doc");
        assert_eq!(id.icon.color, "#3478F6");
    }

    /// A real-LLM identity derivation for the system suite. Ignored by default so
    /// the unit gate stays green without live credentials; run with
    /// `cargo test -- --ignored`. Asserts the model produces a real title and the
    /// allowlisted `airplane` symbol for a flight task — proving the call no
    /// longer truncates to empty content and hits the heuristic fallback.
    #[tokio::test]
    #[ignore = "makes real API call — run with `cargo test -- --ignored`"]
    async fn real_llm_derives_identity_not_fallback() {
        dotenvy::dotenv().ok();
        let client = MoonshotClient::from_env();
        let id = derive_identity(&client, "Book a flight to Tokyo").await;
        assert!(!id.title.trim().is_empty(), "title must be non-empty");
        assert!(
            SYMBOL_ALLOWLIST.contains(&id.icon.symbol.as_str()),
            "symbol '{}' must be allowlisted",
            id.icon.symbol
        );
        // The heuristic fallback for a travel task does not yield "airplane";
        // a real model response does, so this distinguishes the two paths.
        assert_eq!(
            id.icon.symbol, "airplane",
            "expected the model's real 'airplane' symbol for a flight task, got '{}'",
            id.icon.symbol
        );
    }

    /// repair_from_json with fully missing fields falls back to heuristics.
    #[test]
    fn repair_from_json_missing_all_fields() {
        let json = "{}";
        let task = "Plan the sprint backlog";
        let id = repair_from_json(json, task);
        assert!(!id.title.is_empty());
        assert!(SYMBOL_ALLOWLIST.contains(&id.icon.symbol.as_str()));
        assert!(COLOR_PALETTE.contains(&id.icon.color.as_str()));
    }
}
