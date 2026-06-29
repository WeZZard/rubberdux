//! OpenCode Go provider module.
//!
//! OpenCode Go is an AI code-assistant service that exposes an OpenAI Chat
//! Completions-compatible endpoint. This module is the canonical home for the
//! OpenCode Go provider descriptor: the default base URL, default model, wire
//! dialect, and authentication scheme.
//!
//! All defaults are configurable at runtime via the `RUBBERDUX_LLM_*` environment
//! variables; nothing here is permanently hard-coded. The actual live base URL and
//! model id are treated as unverified until confirmed by end-to-end verification
//! (see `TODO(verify-live)` comments).

use crate::provider::{AuthScheme, Dialect, Provider, ProviderDescriptor};

/// Default base URL for the OpenCode Go API.
///
/// TODO(verify-live): confirmed against the live OpenCode Go endpoint during
/// the e2e-verify task; override with `RUBBERDUX_LLM_BASE_URL`.
// Reachable from the binary once wire-host wires selected_from_env; suppress premature dead_code until then.
#[allow(dead_code)]
pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/v1";

/// Default model id for OpenCode Go.
///
/// TODO(verify-live): confirmed against the live endpoint during e2e-verify;
/// override with `RUBBERDUX_LLM_MODEL`.
// Reachable from the binary once wire-host wires selected_from_env; suppress premature dead_code until then.
#[allow(dead_code)]
pub const DEFAULT_MODEL: &str = "grok-code";

/// The canonical OpenCode Go provider descriptor.
///
/// OpenCode Go speaks the OpenAI Chat Completions dialect with Bearer
/// authentication, so any OpenAI-compatible client can drive it without
/// provider-specific quirks.
// Reachable from the binary once wire-host wires selected_from_env; suppress premature dead_code until then.
#[allow(dead_code)]
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        id: Provider::OpenCodeGo,
        default_base_url: DEFAULT_BASE_URL,
        default_model: DEFAULT_MODEL,
        default_dialect: Dialect::OpenAiChatCompletions,
        auth: AuthScheme::Bearer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AuthScheme, Dialect, Provider};

    #[test]
    fn descriptor_has_expected_defaults() {
        let d = descriptor();
        assert_eq!(d.id, Provider::OpenCodeGo);
        assert_eq!(d.default_base_url, DEFAULT_BASE_URL);
        assert_eq!(d.default_model, DEFAULT_MODEL);
        assert_eq!(d.default_dialect, Dialect::OpenAiChatCompletions);
        assert_eq!(d.auth, AuthScheme::Bearer);
    }
}
