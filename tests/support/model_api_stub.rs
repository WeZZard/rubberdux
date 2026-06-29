//! Shared test helpers for the config-selected model provider.
//!
//! The `src/` refactor routes every model-call seam through the neutral
//! [`rubberdux::provider::ModelApi`]. Offline integration/system tests that used
//! to build a concrete client pointed at a mock (or unreachable) URL now build the
//! OpenAI-dialect adapter and hand it across the seam as the object-safe
//! `Arc<dyn ModelApi>` the call sites require. Live tests use
//! `rubberdux::provider::selected_from_env` instead (per the mock-data policy, a
//! real call must not be faked); this helper covers only the offline-adapter
//! construction that was duplicated across many tests.

#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rubberdux::error::Error;
use rubberdux::provider::dialect::openai_chat_completions::OpenAiChatCompletions;
use rubberdux::provider::{
    AuthScheme, ContentBlock, ModelApi, ModelInfo, ModelRequest, ModelResponse, ReasoningPolicy,
    StopReason, Usage,
};

/// Build an OpenAI-dialect [`ModelApi`] adapter pointed at `base_url` serving
/// `model`, returned as the object-safe `Arc<dyn ModelApi>` every seam now
/// depends on. Bearer auth, mirroring the OpenAI providers' descriptor.
pub fn openai_model_api(
    base_url: impl Into<String>,
    model: impl Into<String>,
) -> Arc<dyn ModelApi> {
    Arc::new(OpenAiChatCompletions::new(
        reqwest::Client::new(),
        base_url.into(),
        "test-key".into(),
        model.into(),
        AuthScheme::Bearer,
    ))
}

/// A [`ModelApi`] adapter bound to an unreachable URL, for tests that construct a
/// model-driven component (registry, gateway state, agent tool) but never make a
/// real call.
pub fn dummy_model_api() -> Arc<dyn ModelApi> {
    openai_model_api("http://localhost:0", "test-model")
}

/// A counting [`ModelApi`] that records how many turns it was driven through and
/// answers each with a minimal valid end-of-turn response, no network.
///
/// One instance, handed to BOTH model-call seams (the host agent loop's
/// `Arc<dyn ModelApi>` and the world driver's `&dyn ModelApi`), proves the
/// system locks to a single selected provider across both runtimes: a turn from
/// either seam bumps the SAME `calls` counter, so its final value is the total
/// across both. This is a wiring counter — it asserts which provider instance was
/// reached, not the content of the model's reasoning (mock-data policy).
pub struct CountingModelApi {
    calls: Arc<AtomicUsize>,
    model: String,
}

impl CountingModelApi {
    /// A fresh counting stub serving the fixed alias `counting-stub`, its counter
    /// starting at zero.
    pub fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            model: "counting-stub".to_string(),
        }
    }

    /// How many `turn()` calls this instance has served so far.
    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Default for CountingModelApi {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelApi for CountingModelApi {
    fn turn<'a>(
        &'a self,
        _req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let model_id = self.model.clone();
        Box::pin(async move {
            Ok(ModelResponse {
                blocks: vec![ContentBlock::Text {
                    text: "ok".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                model_id,
                reasoning: ReasoningPolicy::Drop,
                capabilities: serde_json::json!({}),
            })
        })
    }

    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn model(&self) -> &str {
        &self.model
    }
}
