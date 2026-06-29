//! Ollama Cloud provider module.
//!
//! Ollama Cloud exposes an OpenAI Chat Completions-compatible endpoint. This
//! module is the canonical home for the Ollama Cloud provider descriptor: the
//! default base URL, default model, wire dialect, and authentication scheme.
//!
//! **Tool-calling caveat.** The per-App world agent issues tool calls (e.g. the
//! `set_value` surface tool). Ollama Cloud is reached over the OpenAI Chat
//! Completions dialect, and tool calling on Ollama's `/v1` path is
//! best-effort and reportedly unreliable. The default provider (KimiForCoding,
//! Anthropic Messages dialect) supports tool use fully; if Ollama Cloud is
//! selected, tool-dependent world agents may degrade or produce unexpected
//! results. A native Ollama dialect that maps tool-call semantics more
//! faithfully is out of scope. A runtime warning is emitted by
//! `provider::selected_from_env()` whenever Ollama Cloud is selected so
//! operators are aware at startup.
//!
//! All defaults are configurable at runtime via the `RUBBERDUX_LLM_*` environment
//! variables; nothing here is permanently hard-coded. The actual live base URL and
//! model id are treated as unverified until confirmed by end-to-end verification
//! (see `TODO(verify-live)` comments).

use crate::provider::{AuthScheme, Dialect, Provider, ProviderDescriptor};

/// Default base URL for the Ollama Cloud API.
///
/// TODO(verify-live): confirmed against the live Ollama Cloud endpoint during
/// the e2e-verify task; override with `RUBBERDUX_LLM_BASE_URL`.
// Reachable from the binary once wire-host wires selected_from_env; suppress premature dead_code until then.
#[allow(dead_code)]
pub const DEFAULT_BASE_URL: &str = "https://ollama.com/v1";

/// Default model id for Ollama Cloud.
///
/// TODO(verify-live): confirmed against the live endpoint during e2e-verify;
/// override with `RUBBERDUX_LLM_MODEL`.
// Reachable from the binary once wire-host wires selected_from_env; suppress premature dead_code until then.
#[allow(dead_code)]
pub const DEFAULT_MODEL: &str = "gpt-oss:120b";

/// The canonical Ollama Cloud provider descriptor.
///
/// Ollama Cloud speaks the OpenAI Chat Completions dialect with Bearer
/// authentication. See the module-level documentation for the tool-calling
/// caveat that applies when this provider is selected.
// Reachable from the binary once wire-host wires selected_from_env; suppress premature dead_code until then.
#[allow(dead_code)]
pub fn descriptor() -> ProviderDescriptor {
    ProviderDescriptor {
        id: Provider::OllamaCloud,
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
        assert_eq!(d.id, Provider::OllamaCloud);
        assert_eq!(d.default_base_url, DEFAULT_BASE_URL);
        assert_eq!(d.default_model, DEFAULT_MODEL);
        assert_eq!(d.default_dialect, Dialect::OpenAiChatCompletions);
        assert_eq!(d.auth, AuthScheme::Bearer);
    }
}
