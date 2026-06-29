//! The LLM-backed [`Clusterer`] with a lexical Jaccard pre-filter and offline
//! fallback. See `docs/app/merge/clustering.md` for the A/B/C trade-offs and
//! when clustering runs.
//!
//! The mechanism mirrors `crate::app::identity`: a constrained-JSON Kimi for Coding
//! call (here at temperature 0, so the decision is as deterministic as the
//! provider allows) classifies a new conversation summary against candidate App
//! summaries. The Jaccard pre-filter bounds the candidate set offered to the
//! model and is the fallback when the model call fails or returns malformed
//! JSON.

use std::sync::Arc;

use serde::Deserialize;

use crate::agent::runtime::model_bridge::{from_model_response, to_model_request};
use crate::provider::kimi_for_coding::{Message, UserContent, extract_json_object};
use crate::provider::{Effort, ModelApi, Sampling};

use super::{ClusterCandidate, ClusterDecision, Clusterer};

/// Output-token budget for the classify call. The reasoning model spends
/// completion tokens on a visible chain-of-thought before the decision, so the
/// budget must cover both or the JSON answer is truncated to empty content. See
/// `docs/app/merge/clustering.md`.
const CLUSTERING_MAX_TOKENS: u32 = 2048;

/// The maximum number of candidates handed to the LLM after the Jaccard
/// pre-filter ranks them. Bounds the prompt size and cost when the board holds
/// many Apps. See `docs/app/merge/clustering.md` (option C, pre-filter).
pub const MAX_LLM_CANDIDATES: usize = 8;

/// The Jaccard similarity a candidate must clear for the offline fallback to
/// merge into it. Set high so the lexical fallback only joins near-duplicates;
/// anything below this conservatively forms a new App. See
/// `docs/app/merge/clustering.md` (option C, fallback).
pub const JACCARD_JOIN_THRESHOLD: f64 = 0.6;

/// The LLM-backed clusterer. Holds the selected [`ModelApi`] used for the
/// classify call; the Jaccard pre-filter and fallback need no state.
pub struct LlmClusterer {
    client: Arc<dyn ModelApi>,
}

/// The model's classify response shape. `decision` is `"join"` or `"new"`;
/// `index` is the position in the (pre-filtered) candidate list to join.
#[derive(Debug, Deserialize)]
struct LlmClassifyResponse {
    decision: Option<String>,
    index: Option<usize>,
}

impl LlmClusterer {
    /// Construct a clusterer over the selected model API. The same `ModelApi`
    /// the gateway already holds for identity derivation is reused, so no new
    /// state field or dependency is introduced.
    pub fn new(client: Arc<dyn ModelApi>) -> Self {
        Self { client }
    }

    /// Decide for `new_summary` against `candidates`, having already excluded
    /// `user_locked` Apps. Pre-filters with Jaccard, asks the LLM, and falls
    /// back to the best Jaccard candidate when the LLM is unavailable.
    async fn decide(
        &self,
        new_summary: &str,
        candidates: &[ClusterCandidate],
    ) -> ClusterDecision {
        // No candidate to merge into: always a new App.
        if candidates.is_empty() {
            return ClusterDecision::New;
        }

        // Pre-filter: rank by Jaccard and keep the top `MAX_LLM_CANDIDATES`.
        let shortlist = prefilter(new_summary, candidates, MAX_LLM_CANDIDATES);

        match self.classify_via_llm(new_summary, &shortlist).await {
            Some(decision) => decision,
            // Offline fallback: lexical Jaccard against the shortlist.
            None => lexical_fallback(new_summary, &shortlist),
        }
    }

    /// Send the classify request to the model and map its response onto a
    /// [`ClusterDecision`]. Returns `None` on any network / HTTP / parse error
    /// so the caller can fall back to the lexical decision.
    async fn classify_via_llm(
        &self,
        new_summary: &str,
        shortlist: &[ClusterCandidate],
    ) -> Option<ClusterDecision> {
        let messages = vec![system_message(), user_message(new_summary, shortlist)];

        let raw = fetch_raw_json(self.client.as_ref(), messages).await?;
        Some(parse_decision(&raw, shortlist))
    }
}

impl Clusterer for LlmClusterer {
    async fn classify(
        &self,
        new_summary: &str,
        candidates: &[ClusterCandidate],
    ) -> ClusterDecision {
        self.decide(new_summary, candidates).await
    }

    async fn reevaluate(
        &self,
        member_summary: &str,
        current_app_id: &str,
        candidates: &[ClusterCandidate],
    ) -> ClusterDecision {
        // Re-deciding membership compares the drifted summary against the other
        // Apps; staying in the current App is modeled as excluding it from the
        // candidate set and letting `New` mean "no better home was found".
        let others: Vec<ClusterCandidate> = candidates
            .iter()
            .filter(|c| c.app_id != current_app_id)
            .cloned()
            .collect();
        self.decide(member_summary, &others).await
    }
}

// ---------------------------------------------------------------------------
// Lexical Jaccard pre-filter and fallback
// ---------------------------------------------------------------------------

/// Tokenize a summary into a set of lowercase alphanumeric words for Jaccard
/// comparison. Punctuation and case are discarded so "Plan trip!" and
/// "plan trip" share tokens.
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// Jaccard similarity between two summaries: `|A ∩ B| / |A ∪ B|`. Two empty
/// summaries are treated as fully dissimilar (0.0) so an empty new summary never
/// merges by accident.
fn jaccard(a: &str, b: &str) -> f64 {
    let sa = tokenize(a);
    let sb = tokenize(b);
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let intersection = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Rank `candidates` by descending Jaccard similarity to `new_summary` and keep
/// the top `limit`. The pre-filter bounds the LLM prompt; ties keep their
/// original order, so the result is deterministic.
fn prefilter(
    new_summary: &str,
    candidates: &[ClusterCandidate],
    limit: usize,
) -> Vec<ClusterCandidate> {
    let mut scored: Vec<(f64, &ClusterCandidate)> = candidates
        .iter()
        .map(|c| (jaccard(new_summary, &c.summary), c))
        .collect();
    // Stable sort by descending score; equal scores keep input order.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, c)| c.clone())
        .collect()
}

/// The offline fallback: join the best Jaccard candidate only when it clears
/// [`JACCARD_JOIN_THRESHOLD`] (a near-duplicate); otherwise create a new App.
/// This is the deterministic, dependency-free decision used when the LLM call is
/// unavailable. See `docs/app/merge/clustering.md` (option C).
fn lexical_fallback(new_summary: &str, candidates: &[ClusterCandidate]) -> ClusterDecision {
    let best = candidates
        .iter()
        .map(|c| (jaccard(new_summary, &c.summary), c))
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    match best {
        Some((score, candidate)) if score >= JACCARD_JOIN_THRESHOLD => {
            ClusterDecision::Join {
                app_id: candidate.app_id.clone(),
            }
        }
        // Below threshold or no candidate: conservative default is a new App.
        _ => ClusterDecision::New,
    }
}

// ---------------------------------------------------------------------------
// LLM prompt + response handling
// ---------------------------------------------------------------------------

/// The system message instructing the model to classify a new conversation
/// summary against numbered candidate summaries, returning constrained JSON.
fn system_message() -> Message {
    let content = "You are a conversation-clustering assistant. \
        You are given a new conversation summary and a numbered list of existing \
        app summaries. Decide whether the new conversation belongs with one of \
        the existing apps (because they share the same task or topic) or should \
        start a new app.\n\
        \n\
        Return ONLY a JSON object:\n\
        - To join app number N: {\"decision\":\"join\",\"index\":N}\n\
        - To start a new app:   {\"decision\":\"new\"}\n\
        \n\
        Be conservative: only join when the topics clearly match. When in doubt, \
        choose \"new\". No explanation, no markdown fences."
        .to_owned();
    Message::System { content }
}

/// The user message listing the new summary and the candidate summaries by
/// zero-based index, matching the indices the model returns.
fn user_message(new_summary: &str, candidates: &[ClusterCandidate]) -> Message {
    let mut listing = String::new();
    for (i, candidate) in candidates.iter().enumerate() {
        listing.push_str(&format!("{}: {}\n", i, candidate.summary));
    }
    let content = format!(
        "New conversation summary:\n{new_summary}\n\nExisting apps:\n{listing}",
        new_summary = new_summary,
        listing = listing,
    );
    Message::User {
        content: UserContent::Text(content),
    }
}

/// Run the classify turn through the selected [`ModelApi`] and return the text
/// content of the assistant response. Returns `None` on any provider error so
/// the lexical fallback runs cleanly. The JSON shape is requested at the prompt
/// level (see `system_message`), so no tools are offered. Mirrors
/// `crate::app::identity`'s `fetch_raw_json`.
async fn fetch_raw_json(client: &dyn ModelApi, messages: Vec<Message>) -> Option<String> {
    let sampling = Sampling {
        model: client.model().to_owned(),
        max_tokens: CLUSTERING_MAX_TOKENS,
        effort: Effort::Medium,
    };
    let request = to_model_request(&messages, None, sampling);

    let response = match client.turn(&request).await {
        Ok(resp) => from_model_response(resp),
        Err(e) => {
            log::warn!("clustering: LLM call failed: {e}");
            return None;
        }
    };

    let message = response.choices.into_iter().next()?.message;
    let content = message.content_text().trim();
    // Empty/whitespace content (e.g. a truncated reasoning response) yields `None`
    // so the lexical fallback runs cleanly rather than hitting the parse-error
    // path with an empty string.
    if content.is_empty() {
        return None;
    }
    Some(content.to_owned())
}

/// Parse the model's JSON response into a [`ClusterDecision`], validating the
/// index against the shortlist. A malformed response, an out-of-range index, or
/// a missing index on a "join" decision all conservatively become `New`.
fn parse_decision(json: &str, shortlist: &[ClusterCandidate]) -> ClusterDecision {
    // Extract the first balanced JSON object, tolerating markdown fences or
    // surrounding prose the reasoning model may emit around the answer.
    let object = match extract_json_object(json) {
        Some(o) => o,
        None => {
            log::warn!("clustering: no JSON object in LLM response, defaulting to new");
            return ClusterDecision::New;
        }
    };

    let parsed: Result<LlmClassifyResponse, _> = serde_json::from_str(object);
    let response = match parsed {
        Ok(r) => r,
        Err(e) => {
            log::warn!("clustering: malformed JSON from LLM ({e}), defaulting to new");
            return ClusterDecision::New;
        }
    };

    match response.decision.as_deref() {
        Some("join") => match response.index.and_then(|i| shortlist.get(i)) {
            Some(candidate) => ClusterDecision::Join {
                app_id: candidate.app_id.clone(),
            },
            None => {
                log::warn!("clustering: join index out of range, defaulting to new");
                ClusterDecision::New
            }
        },
        _ => ClusterDecision::New,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_client(base_url: &str) -> Arc<dyn ModelApi> {
        use crate::provider::AuthScheme;
        use crate::provider::dialect::openai_chat_completions::OpenAiChatCompletions;
        Arc::new(OpenAiChatCompletions::new(
            reqwest::Client::new(),
            base_url.to_owned(),
            "test-key".to_owned(),
            "test-model".to_owned(),
            AuthScheme::Bearer,
        ))
    }

    fn candidate(id: &str, summary: &str) -> ClusterCandidate {
        ClusterCandidate {
            app_id: id.to_owned(),
            summary: summary.to_owned(),
        }
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
                "completion_tokens": 5,
                "total_tokens": 15,
                "cached_tokens": 0
            }
        })
    }

    // -- Lexical pre-filter / fallback (no network) ------------------------

    #[test]
    fn jaccard_identical_summaries_is_one() {
        assert!((jaccard("plan the trip", "plan the trip") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_disjoint_summaries_is_zero() {
        assert_eq!(jaccard("debug the parser", "buy groceries milk"), 0.0);
    }

    #[test]
    fn jaccard_empty_is_zero() {
        assert_eq!(jaccard("", "anything here"), 0.0);
        assert_eq!(jaccard("anything here", ""), 0.0);
    }

    /// The fallback joins a near-duplicate candidate (Jaccard clears the
    /// threshold).
    #[test]
    fn lexical_fallback_joins_near_duplicate() {
        let candidates = vec![
            candidate("app-1", "plan the Tokyo trip itinerary"),
            candidate("app-2", "fix the failing auth tests"),
        ];
        let decision = lexical_fallback("plan the Tokyo trip itinerary today", &candidates);
        assert_eq!(
            decision,
            ClusterDecision::Join {
                app_id: "app-1".to_owned()
            }
        );
    }

    /// The fallback creates a new App when the best candidate is below the
    /// threshold.
    #[test]
    fn lexical_fallback_new_when_below_threshold() {
        let candidates = vec![
            candidate("app-1", "plan the Tokyo trip"),
            candidate("app-2", "fix the failing auth tests"),
        ];
        let decision = lexical_fallback("write quarterly revenue report", &candidates);
        assert_eq!(decision, ClusterDecision::New);
    }

    /// The fallback creates a new App when there are no candidates at all.
    #[test]
    fn lexical_fallback_new_when_no_candidates() {
        assert_eq!(lexical_fallback("anything", &[]), ClusterDecision::New);
    }

    /// The pre-filter ranks the most lexically similar candidate first and keeps
    /// at most `limit`.
    #[test]
    fn prefilter_ranks_and_bounds() {
        let candidates = vec![
            candidate("a", "buy groceries"),
            candidate("b", "plan the Tokyo trip itinerary"),
            candidate("c", "fix tests"),
        ];
        let shortlist = prefilter("plan the Tokyo trip", &candidates, 2);
        assert_eq!(shortlist.len(), 2);
        // The most similar ("b") must rank first.
        assert_eq!(shortlist[0].app_id, "b");
    }

    // -- LLM classify step (mocked client, like identity) ------------------

    #[tokio::test]
    async fn llm_join_decision_maps_index_to_app_id() {
        let server = MockServer::start().await;
        let body = serde_json::to_string(&chat_response_body(
            r#"{"decision":"join","index":1}"#,
        ))
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let clusterer = LlmClusterer::new(make_client(&server.uri()));
        // The new summary shares no tokens with either candidate, so the lexical
        // pre-filter scores both 0.0 and keeps them in input order; index 1 then
        // deterministically resolves to "app-2", proving the index→app_id map.
        let candidates = vec![
            candidate("app-1", "plan the Tokyo trip"),
            candidate("app-2", "review the pull request"),
        ];
        let decision = clusterer
            .classify("xyzzy quux frobnicate", &candidates)
            .await;
        assert_eq!(
            decision,
            ClusterDecision::Join {
                app_id: "app-2".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn llm_new_decision_creates_new_app() {
        let server = MockServer::start().await;
        let body =
            serde_json::to_string(&chat_response_body(r#"{"decision":"new"}"#)).unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let clusterer = LlmClusterer::new(make_client(&server.uri()));
        let candidates = vec![candidate("app-1", "plan the Tokyo trip")];
        let decision = clusterer
            .classify("write the quarterly report", &candidates)
            .await;
        assert_eq!(decision, ClusterDecision::New);
    }

    /// An HTTP failure falls back to the lexical decision: a near-duplicate
    /// still joins.
    #[tokio::test]
    async fn llm_http_failure_falls_back_to_lexical_join() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let clusterer = LlmClusterer::new(make_client(&server.uri()));
        let candidates = vec![candidate("app-1", "plan the Tokyo trip itinerary")];
        let decision = clusterer
            .classify("plan the Tokyo trip itinerary now", &candidates)
            .await;
        assert_eq!(
            decision,
            ClusterDecision::Join {
                app_id: "app-1".to_owned()
            }
        );
    }

    /// Malformed JSON from the model conservatively yields a new App.
    #[tokio::test]
    async fn llm_malformed_json_defaults_to_new() {
        let server = MockServer::start().await;
        let body =
            serde_json::to_string(&chat_response_body("not json {{{")).unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let clusterer = LlmClusterer::new(make_client(&server.uri()));
        let candidates = vec![candidate("app-1", "unrelated topic entirely")];
        let decision = clusterer.classify("brand new subject", &candidates).await;
        assert_eq!(decision, ClusterDecision::New);
    }

    /// `reevaluate` excludes the current App from the candidate set.
    #[tokio::test]
    async fn reevaluate_excludes_current_app() {
        let server = MockServer::start().await;
        // The model is told to join index 0; with the current App excluded,
        // index 0 of the remaining list is "app-2".
        let body = serde_json::to_string(&chat_response_body(
            r#"{"decision":"join","index":0}"#,
        ))
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let clusterer = LlmClusterer::new(make_client(&server.uri()));
        let candidates = vec![
            candidate("app-1", "the current home app"),
            candidate("app-2", "the better matching app"),
        ];
        let decision = clusterer
            .reevaluate("drifted summary", "app-1", &candidates)
            .await;
        assert_eq!(
            decision,
            ClusterDecision::Join {
                app_id: "app-2".to_owned()
            }
        );
    }

    /// A real-LLM integration test for the later system suite. Ignored by
    /// default so the unit gate stays green without live credentials; run with
    /// `cargo test -- --ignored`. See `docs/app/merge/clustering.md`.
    #[tokio::test]
    #[ignore = "makes real API call — run with `cargo test -- --ignored`"]
    async fn real_llm_classifies_related_conversation_as_join() {
        let clusterer = LlmClusterer::new(Arc::from(
            crate::provider::selected_from_env().expect("provider selection"),
        ));
        let candidates = vec![
            candidate("trip", "Plan a one-week trip to Tokyo: flights, hotels, itinerary"),
            candidate("taxes", "File the quarterly business tax return"),
        ];
        let decision = clusterer
            .classify(
                "Book flights and a hotel for the Tokyo vacation next month",
                &candidates,
            )
            .await;
        assert_eq!(
            decision,
            ClusterDecision::Join {
                app_id: "trip".to_owned()
            },
            "a clearly related travel conversation should join the trip app"
        );
    }
}
