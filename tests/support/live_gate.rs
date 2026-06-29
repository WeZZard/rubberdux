//! The human-run gate for live-LLM and real-subprocess tests.
//!
//! The project's mock-data policy (see root `CLAUDE.md`) forbids mocked data
//! in integration, system, and end-to-end tests: those must call the real
//! model. To keep a developer's `cargo test` green without live credentials,
//! every test that needs the real LLM calls [`live_llm_credentials`] first and
//! returns early with a clear, printed reason when the credentials are absent.
//! When `RUBBERDUX_LLM_API_KEY` (and optionally a base URL/model) are present —
//! the human-run suite — the test proceeds against the real provider via
//! `KimiForCodingClient::from_env`.
//!
//! This mirrors the env vars `KimiForCodingClient::from_env` reads, so a test gated
//! here and a client built with `from_env` always agree on whether the live
//! provider is reachable.

#![allow(dead_code)]

/// The credentials a live-LLM test needs, read from the same environment
/// `KimiForCodingClient::from_env` consults. `Some` only when an API key is set, so
/// the gate never sends an unauthenticated request the provider would reject.
pub struct LiveLlmCredentials {
    pub base_url: String,
    pub model: String,
}

/// Resolve live-LLM credentials from the environment, or `None` when this is a
/// developer run without them. The required signal is a non-empty
/// `RUBBERDUX_LLM_API_KEY`; base URL and model fall back to the same defaults
/// `KimiForCodingClient::from_env` uses so the gate and the client stay in lockstep.
pub fn live_llm_credentials() -> Option<LiveLlmCredentials> {
    let api_key = std::env::var("RUBBERDUX_LLM_API_KEY").ok()?;
    if api_key.trim().is_empty() {
        return None;
    }
    let base_url = std::env::var("RUBBERDUX_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.moonshot.cn/v1".into());
    let model =
        std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());
    Some(LiveLlmCredentials { base_url, model })
}

/// Print a uniform skip notice and return `true` when live-LLM credentials are
/// absent, so a gated test can `if skip_without_live_llm("name") { return; }`.
/// The message names the test and the env var the human must set, so a reader
/// of a green local run sees exactly which checks the live suite still owes.
pub fn skip_without_live_llm(test_name: &str) -> bool {
    if live_llm_credentials().is_some() {
        return false;
    }
    eprintln!(
        "SKIP {test_name}: live-LLM credentials absent. Set RUBBERDUX_LLM_API_KEY \
         (and optionally RUBBERDUX_LLM_BASE_URL / RUBBERDUX_LLM_MODEL) to run this \
         test against the real provider — this is the human-gated live suite."
    );
    true
}
