//! provider — the provider domain core.
//!
//! Defines the neutral [`ModelApi`] a turn is driven through, the neutral
//! request/response vocabulary that crosses the model boundary, the
//! provider/dialect descriptor model, and config-from-env selection.
//!
//! This is the canonical home for the model result types (`StopReason`,
//! `Usage`, `ReasoningPolicy`); `agent::world::inputs` re-exports them so the
//! event log and `ModelMeta` keep a single definition with no drift.

pub mod dialect;
pub mod kimi_for_coding;
pub mod ollama_cloud;
pub mod opencode_go;

use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Opaque structured JSON: tool inputs, tool input schemas, and capability
/// metadata. Aliased so the domain reads as "a tool's input is JSON" rather than
/// leaking the serde library name.
pub type Json = serde_json::Value;

// ---------------------------------------------------------------------------
// Relocated canonical result types — formerly in agent/world/inputs.rs
//
// These are the model-call result vocabulary; the provider is their canonical
// home because they describe what a model returns. `agent::world::inputs`
// re-exports them, so persisted events/snapshots are byte-unchanged (the serde
// representation below is preserved exactly from the old location).
// ---------------------------------------------------------------------------

/// Token accounting for one inference. Folded into per-entity budgets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Why the model stopped. Serialises snake_case to match the Anthropic
/// `stop_reason` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    PauseTurn,
}

/// How reasoning blocks were round-tripped for a call, recorded so replay
/// reconstructs the SAME History deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPolicy {
    Echo,
    Drop,
    MustEcho,
}

// ---------------------------------------------------------------------------
// ModelApi — the neutral contract a turn is driven through
// ---------------------------------------------------------------------------

/// The neutral model API every dialect adapter implements. Object-safe so the
/// selected adapter is held as `Box<dyn ModelApi>`. Async is expressed as a
/// returns-future signature (rather than a macro) because the crate carries no
/// async-trait dependency; this keeps the trait object-safe without adding one.
pub trait ModelApi: Send + Sync {
    /// Run one model turn against the neutral request.
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>>;

    /// List the models the provider exposes.
    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>>;

    /// The configured model alias for this adapter.
    fn model(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Neutral request / response vocabulary
// ---------------------------------------------------------------------------

/// A neutral model request: the assembled system prompt, the conversation, the
/// offered tools, and the sampling parameters. Dialect adapters translate this
/// into their wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub system: String,
    pub messages: Vec<ContentMessage>,
    pub tools: Vec<ToolSpec>,
    pub sampling: Sampling,
}

/// One conversation message: a role and its ordered content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Neutral conversation role. Serialises lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A neutral content block — the explicit, dialect-independent union a request
/// or response carries. Dialect adapters map this to/from their wire schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Json,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
}

/// Neutral image source: inline base64 bytes with a media type, or a URL. Kept
/// minimal — dialect adapters widen this to their wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// One tool DECLARATION offered to a model call: `{ name, description,
/// input_schema }` where `input_schema` is the JSON Schema the model's tool
/// input must satisfy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Json,
}

/// Reasoning/output effort. A neutral mirror of the world's effort knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// Neutral sampling parameters for one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sampling {
    pub model: String,
    pub max_tokens: u32,
    pub effort: Effort,
}

/// A neutral model response: the assistant blocks, why it stopped, token usage,
/// the effective model id, the reasoning round-trip policy, and opaque
/// capability metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub blocks: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub model_id: String,
    pub reasoning: ReasoningPolicy,
    pub capabilities: Json,
}

/// One entry from a provider's model listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub owned_by: Option<String>,
    pub context_length: Option<u32>,
}

// ---------------------------------------------------------------------------
// Selection model — Provider / Dialect / descriptor table
// ---------------------------------------------------------------------------

/// The provider chosen by config. The string forms are the stable config values
/// (`kimi-for-coding` / `opencode-go` / `ollama-cloud`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    KimiForCoding,
    OpenCodeGo,
    OllamaCloud,
}

impl FromStr for Provider {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "kimi-for-coding" => Ok(Provider::KimiForCoding),
            "opencode-go" => Ok(Provider::OpenCodeGo),
            "ollama-cloud" => Ok(Provider::OllamaCloud),
            other => Err(Error::Provider(format!("unknown provider: {other}"))),
        }
    }
}

/// The wire dialect a provider speaks. The string forms are the config values
/// (`openai` / `anthropic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    OpenAiChatCompletions,
    AnthropicMessages,
}

impl FromStr for Dialect {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openai" => Ok(Dialect::OpenAiChatCompletions),
            "anthropic" => Ok(Dialect::AnthropicMessages),
            other => Err(Error::Provider(format!("unknown dialect: {other}"))),
        }
    }
}

impl Provider {
    /// Canonical kebab-case identifier, matching the serde `rename_all =
    /// "kebab-case"` wire form. Used in the `GET /api/v1/provider` response.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::KimiForCoding => "kimi-for-coding",
            Provider::OpenCodeGo => "opencode-go",
            Provider::OllamaCloud => "ollama-cloud",
        }
    }
}

impl Dialect {
    /// Canonical kebab-case identifier used in the `GET /api/v1/provider`
    /// response (`"anthropic-messages"` or `"openai-chat-completions"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Dialect::AnthropicMessages => "anthropic-messages",
            Dialect::OpenAiChatCompletions => "openai-chat-completions",
        }
    }
}

/// How a provider authenticates a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// Anthropic's `x-api-key: <key>` header.
    AnthropicXApiKey,
}

/// Internal static descriptor / lookup-table entry for one provider.
///
/// Carries the per-provider defaults (base URL, model, dialect, auth scheme)
/// as `&'static str` fields so the table lives entirely in the binary without
/// heap allocation.
///
/// **Not a serde boundary type.** `&'static str` fields cannot implement
/// `Deserialize<'de>` for a `'static` borrow without redesigning the table to
/// use owned `String`, which is unnecessary because this struct is never
/// serialised. The serde boundary types are [`ResolvedSelection`] (after env
/// overrides are applied) and the neutral request/response vocabulary
/// ([`ModelRequest`], [`ModelResponse`], [`ContentBlock`], etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub id: Provider,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
    pub default_dialect: Dialect,
    pub auth: AuthScheme,
}

// TODO(verify-live): the base URL and default model below are UNVERIFIED
// placeholders confirmed against the live Kimi endpoint in a later task. They
// are per-provider defaults, intentionally overridable by RUBBERDUX_LLM_* env
// vars. The OpenCode Go and Ollama Cloud defaults live in their own modules.
const KIMI_BASE_URL: &str = "https://api.moonshot.ai/anthropic";
const KIMI_MODEL: &str = "kimi-for-coding";

/// The descriptor table: the per-provider defaults. Default dialects:
/// KimiForCoding speaks Anthropic Messages; OpenCodeGo and OllamaCloud speak
/// OpenAI Chat Completions. The OpenCode Go and Ollama Cloud descriptors are
/// sourced from their own modules (`opencode_go::descriptor()` and
/// `ollama_cloud::descriptor()`), which are the canonical homes for those
/// providers' defaults.
pub fn descriptor(p: Provider) -> ProviderDescriptor {
    match p {
        Provider::KimiForCoding => ProviderDescriptor {
            id: Provider::KimiForCoding,
            default_base_url: KIMI_BASE_URL,
            default_model: KIMI_MODEL,
            default_dialect: Dialect::AnthropicMessages,
            auth: AuthScheme::AnthropicXApiKey,
        },
        Provider::OpenCodeGo => opencode_go::descriptor(),
        Provider::OllamaCloud => ollama_cloud::descriptor(),
    }
}

/// A fully resolved selection: the descriptor defaults with any per-field
/// `RUBBERDUX_LLM_*` overrides applied. This is the value a dialect adapter is
/// constructed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSelection {
    pub provider: Provider,
    pub dialect: Dialect,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub auth: AuthScheme,
}

// ---------------------------------------------------------------------------
// config-from-env selection
// ---------------------------------------------------------------------------

/// Resolve a [`ResolvedSelection`] from the environment.
///
/// `RUBBERDUX_LLM_PROVIDER` selects the descriptor (absent → `kimi-for-coding`
/// for back-compat; an INVALID value is an error, never a silent default). Each
/// further `RUBBERDUX_LLM_*` var overrides only its own field:
/// `RUBBERDUX_LLM_DIALECT` (`openai`|`anthropic`), `RUBBERDUX_LLM_BASE_URL`,
/// `RUBBERDUX_LLM_API_KEY`, `RUBBERDUX_LLM_MODEL`. An unknown provider or
/// dialect returns `Err`.
pub fn resolve_from_env() -> Result<ResolvedSelection, Error> {
    let provider = match std::env::var("RUBBERDUX_LLM_PROVIDER") {
        Ok(raw) => raw.parse::<Provider>()?,
        // Absent (not set) → the back-compat default. A set-but-invalid value
        // takes the `Ok(raw)` arm above and errors on parse.
        Err(_) => Provider::KimiForCoding,
    };
    let desc = descriptor(provider);

    let dialect = match std::env::var("RUBBERDUX_LLM_DIALECT") {
        Ok(raw) => raw.parse::<Dialect>()?,
        Err(_) => desc.default_dialect,
    };
    let base_url = std::env::var("RUBBERDUX_LLM_BASE_URL")
        .unwrap_or_else(|_| desc.default_base_url.to_string());
    let api_key = std::env::var("RUBBERDUX_LLM_API_KEY").unwrap_or_default();
    let model =
        std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| desc.default_model.to_string());

    Ok(ResolvedSelection {
        provider,
        dialect,
        base_url,
        api_key,
        model,
        auth: desc.auth,
    })
}

/// Resolve from the environment, construct the matching dialect adapter, and
/// return both the [`ResolvedSelection`] (for provider/model/dialect metadata)
/// and the adapter. One-provider invariant: callers that need both the resolved
/// metadata and the adapter call this once; `selected_from_env` routes through
/// it so no second resolution is needed.
pub fn select_from_env() -> Result<(ResolvedSelection, Box<dyn ModelApi>), Error> {
    let resolved = resolve_from_env()?;

    // Warn operators that tool-dependent world agents may degrade when Ollama
    // Cloud is selected, because tool calling over the OpenAI Chat Completions
    // dialect on Ollama's /v1 path is best-effort and reportedly unreliable.
    // See `ollama_cloud` module documentation for details. A native Ollama
    // dialect is out of scope.
    if resolved.provider == Provider::OllamaCloud {
        log::warn!(
            "provider: Ollama Cloud selected — tool calling over the OpenAI \
             Chat Completions dialect is best-effort/unreliable on Ollama; \
             tool-dependent world agents may degrade. A native Ollama dialect \
             is out of scope."
        );
    }

    // Honour the optional user-agent override, mirroring the other env-built
    // clients in this crate.
    let mut builder = reqwest::ClientBuilder::new();
    if let Ok(user_agent) = std::env::var("RUBBERDUX_LLM_USER_AGENT") {
        builder = builder.user_agent(user_agent);
    }
    let http = builder.build()?;

    let adapter: Box<dyn ModelApi> = match resolved.dialect {
        Dialect::OpenAiChatCompletions => {
            Box::new(dialect::openai_chat_completions::OpenAiChatCompletions::new(
                http,
                resolved.base_url.clone(),
                resolved.api_key.clone(),
                resolved.model.clone(),
                resolved.auth,
            ))
        }
        Dialect::AnthropicMessages => {
            Box::new(dialect::anthropic_messages::AnthropicMessages::new(
                http,
                resolved.base_url.clone(),
                resolved.api_key.clone(),
                resolved.model.clone(),
                resolved.auth,
            ))
        }
    };
    Ok((resolved, adapter))
}

/// Resolve from the environment and construct the matching dialect adapter,
/// returning it as a `Box<dyn ModelApi>`. Routes through [`select_from_env`]
/// so the single env resolution is shared.
pub fn selected_from_env() -> Result<Box<dyn ModelApi>, Error> {
    select_from_env().map(|(_, api)| api)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Every `RUBBERDUX_LLM_*` key the selection reads; saved/cleared/restored
    /// around each env-driven test.
    const ENV_KEYS: [&str; 6] = [
        "RUBBERDUX_LLM_PROVIDER",
        "RUBBERDUX_LLM_DIALECT",
        "RUBBERDUX_LLM_BASE_URL",
        "RUBBERDUX_LLM_API_KEY",
        "RUBBERDUX_LLM_MODEL",
        "RUBBERDUX_LLM_USER_AGENT",
    ];

    /// Clear all selection env vars, set the given ones, run `f`, then restore
    /// the original environment. `#[serial(llm_env)]` keeps these tests from
    /// racing each other (single-threaded env discipline).
    fn with_clean_env<T>(set: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        let saved: Vec<(&str, Option<String>)> =
            ENV_KEYS.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        // SAFETY: serialised via #[serial(llm_env)]; no other thread reads these.
        unsafe {
            for k in ENV_KEYS {
                std::env::remove_var(k);
            }
            for (k, v) in set {
                std::env::set_var(k, v);
            }
        }
        let out = f();
        // SAFETY: serialised via #[serial(llm_env)]; no other thread reads these.
        unsafe {
            for (k, v) in &saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        out
    }

    // -- V1.1 — selected_from_env binds to the chosen provider's defaults ------

    #[test]
    #[serial(llm_env)]
    fn selected_from_env_binds_to_provider_defaults() {
        with_clean_env(&[("RUBBERDUX_LLM_PROVIDER", "kimi-for-coding")], || {
            let desc = descriptor(Provider::KimiForCoding);
            let resolved = resolve_from_env().expect("resolve");

            assert_eq!(resolved.provider, Provider::KimiForCoding);
            assert_eq!(resolved.dialect, desc.default_dialect);
            assert_eq!(resolved.dialect, Dialect::AnthropicMessages);
            assert_eq!(resolved.base_url, desc.default_base_url);
            assert_eq!(resolved.model, desc.default_model);
            assert_eq!(resolved.auth, desc.auth);
            assert_eq!(resolved.auth, AuthScheme::AnthropicXApiKey);

            // The boxed adapter is bound to the default model.
            let adapter = selected_from_env().expect("select");
            assert_eq!(adapter.model(), desc.default_model);
        });
    }

    /// [V1.1] Selecting `opencode-go` yields the OpenAI dialect, Bearer auth,
    /// and OpenCode Go's module defaults.
    #[test]
    #[serial(llm_env)]
    fn selected_from_env_binds_opencode_go_to_openai_defaults() {
        with_clean_env(&[("RUBBERDUX_LLM_PROVIDER", "opencode-go")], || {
            let resolved = resolve_from_env().expect("resolve");

            assert_eq!(resolved.provider, Provider::OpenCodeGo);
            assert_eq!(resolved.dialect, Dialect::OpenAiChatCompletions);
            assert_eq!(resolved.base_url, opencode_go::DEFAULT_BASE_URL);
            assert_eq!(resolved.model, opencode_go::DEFAULT_MODEL);
            assert_eq!(resolved.auth, AuthScheme::Bearer);

            let adapter = selected_from_env().expect("select");
            assert_eq!(adapter.model(), opencode_go::DEFAULT_MODEL);
        });
    }

    /// [V1.1] Selecting `ollama-cloud` yields the OpenAI dialect, Bearer auth,
    /// and Ollama Cloud's module defaults.
    #[test]
    #[serial(llm_env)]
    fn selected_from_env_binds_ollama_cloud_to_openai_defaults() {
        with_clean_env(&[("RUBBERDUX_LLM_PROVIDER", "ollama-cloud")], || {
            let resolved = resolve_from_env().expect("resolve");

            assert_eq!(resolved.provider, Provider::OllamaCloud);
            assert_eq!(resolved.dialect, Dialect::OpenAiChatCompletions);
            assert_eq!(resolved.base_url, ollama_cloud::DEFAULT_BASE_URL);
            assert_eq!(resolved.model, ollama_cloud::DEFAULT_MODEL);
            assert_eq!(resolved.auth, AuthScheme::Bearer);

            let adapter = selected_from_env().expect("select");
            assert_eq!(adapter.model(), ollama_cloud::DEFAULT_MODEL);
        });
    }

    /// The concrete adapter accessors (`model()`/`base_url()`/`dialect()`) — the
    /// inherent introspection a selected adapter exposes — return what they were
    /// constructed with. Covers V1.1's "the returned adapter's base_url/dialect".
    #[test]
    fn dialect_adapters_expose_accessors() {
        let http = reqwest::Client::new();
        let anthropic = dialect::anthropic_messages::AnthropicMessages::new(
            http.clone(),
            "https://base.anthropic.test/v1".into(),
            "key-a".into(),
            "model-a".into(),
            AuthScheme::AnthropicXApiKey,
        );
        assert_eq!(anthropic.model(), "model-a");
        assert_eq!(anthropic.base_url(), "https://base.anthropic.test/v1");
        assert_eq!(anthropic.dialect(), Dialect::AnthropicMessages);

        let openai = dialect::openai_chat_completions::OpenAiChatCompletions::new(
            http,
            "https://base.openai.test/v1".into(),
            "key-o".into(),
            "model-o".into(),
            AuthScheme::Bearer,
        );
        assert_eq!(openai.model(), "model-o");
        assert_eq!(openai.base_url(), "https://base.openai.test/v1");
        assert_eq!(openai.dialect(), Dialect::OpenAiChatCompletions);
    }

    /// Absent provider → back-compat default (`kimi-for-coding`).
    #[test]
    #[serial(llm_env)]
    fn absent_provider_defaults_to_kimi_for_back_compat() {
        with_clean_env(&[], || {
            let resolved = resolve_from_env().expect("resolve");
            assert_eq!(resolved.provider, Provider::KimiForCoding);
            assert_eq!(resolved.dialect, Dialect::AnthropicMessages);
        });
    }

    // -- V1.2 — per-field overrides change only their own field ----------------

    #[test]
    #[serial(llm_env)]
    fn per_field_overrides_only_their_own_field() {
        let desc = descriptor(Provider::OllamaCloud);

        // MODEL override leaves base_url/dialect/auth at descriptor defaults.
        with_clean_env(
            &[
                ("RUBBERDUX_LLM_PROVIDER", "ollama-cloud"),
                ("RUBBERDUX_LLM_MODEL", "custom-model-x"),
            ],
            || {
                let resolved = resolve_from_env().expect("resolve");
                assert_eq!(resolved.model, "custom-model-x");
                assert_eq!(resolved.base_url, desc.default_base_url);
                assert_eq!(resolved.dialect, desc.default_dialect);
                assert_eq!(resolved.auth, desc.auth);

                let adapter = selected_from_env().expect("select");
                assert_eq!(adapter.model(), "custom-model-x");
            },
        );

        // BASE_URL override leaves model/dialect at defaults.
        with_clean_env(
            &[
                ("RUBBERDUX_LLM_PROVIDER", "ollama-cloud"),
                ("RUBBERDUX_LLM_BASE_URL", "https://override.example.test/v1"),
            ],
            || {
                let resolved = resolve_from_env().expect("resolve");
                assert_eq!(resolved.base_url, "https://override.example.test/v1");
                assert_eq!(resolved.model, desc.default_model);
                assert_eq!(resolved.dialect, desc.default_dialect);
            },
        );

        // API_KEY override leaves model/base_url at defaults.
        with_clean_env(
            &[
                ("RUBBERDUX_LLM_PROVIDER", "ollama-cloud"),
                ("RUBBERDUX_LLM_API_KEY", "sk-override-123"),
            ],
            || {
                let resolved = resolve_from_env().expect("resolve");
                assert_eq!(resolved.api_key, "sk-override-123");
                assert_eq!(resolved.model, desc.default_model);
                assert_eq!(resolved.base_url, desc.default_base_url);
            },
        );
    }

    // -- V1.3 — dialect override flips Kimi's default Anthropic to OpenAI ------

    #[test]
    #[serial(llm_env)]
    fn dialect_override_flips_kimi_anthropic_to_openai() {
        with_clean_env(
            &[
                ("RUBBERDUX_LLM_PROVIDER", "kimi-for-coding"),
                ("RUBBERDUX_LLM_DIALECT", "openai"),
            ],
            || {
                let desc = descriptor(Provider::KimiForCoding);
                assert_eq!(desc.default_dialect, Dialect::AnthropicMessages);

                let resolved = resolve_from_env().expect("resolve");
                // Only the dialect changed; base_url/model stay Kimi defaults.
                assert_eq!(resolved.dialect, Dialect::OpenAiChatCompletions);
                assert_eq!(resolved.base_url, desc.default_base_url);
                assert_eq!(resolved.model, desc.default_model);

                // selected_from_env constructs the OpenAI adapter, bound to the
                // Kimi default model.
                let adapter = selected_from_env().expect("select");
                assert_eq!(adapter.model(), desc.default_model);
            },
        );
    }

    // -- V1.4 — unknown provider/dialect errors, never panics or defaults -----

    #[test]
    #[serial(llm_env)]
    fn unknown_provider_or_dialect_errors() {
        with_clean_env(&[("RUBBERDUX_LLM_PROVIDER", "does-not-exist")], || {
            assert!(resolve_from_env().is_err());
            assert!(selected_from_env().is_err());
        });

        with_clean_env(
            &[
                ("RUBBERDUX_LLM_PROVIDER", "kimi-for-coding"),
                ("RUBBERDUX_LLM_DIALECT", "not-a-dialect"),
            ],
            || {
                assert!(resolve_from_env().is_err());
                assert!(selected_from_env().is_err());
            },
        );

        // FromStr directly: unknown values are errors, not panics.
        assert!("nope".parse::<Provider>().is_err());
        assert!("nope".parse::<Dialect>().is_err());
        // The known values parse.
        assert_eq!("opencode-go".parse::<Provider>().unwrap(), Provider::OpenCodeGo);
        assert_eq!("anthropic".parse::<Dialect>().unwrap(), Dialect::AnthropicMessages);
    }

    // -- Relocation — serde representation preserved byte-for-byte ------------

    /// The relocated result types keep their exact serde wire forms, so the
    /// event log/snapshots that persist them stay byte-identical.
    #[test]
    fn relocated_types_preserve_serde_representation() {
        assert_eq!(serde_json::to_string(&StopReason::EndTurn).unwrap(), "\"end_turn\"");
        assert_eq!(serde_json::to_string(&StopReason::ToolUse).unwrap(), "\"tool_use\"");
        assert_eq!(serde_json::to_string(&StopReason::MaxTokens).unwrap(), "\"max_tokens\"");
        assert_eq!(serde_json::to_string(&StopReason::Refusal).unwrap(), "\"refusal\"");
        assert_eq!(serde_json::to_string(&StopReason::PauseTurn).unwrap(), "\"pause_turn\"");

        assert_eq!(serde_json::to_string(&ReasoningPolicy::Echo).unwrap(), "\"echo\"");
        assert_eq!(serde_json::to_string(&ReasoningPolicy::Drop).unwrap(), "\"drop\"");
        assert_eq!(
            serde_json::to_string(&ReasoningPolicy::MustEcho).unwrap(),
            "\"must_echo\""
        );

        assert_eq!(
            serde_json::to_string(&Usage {
                input_tokens: 1,
                output_tokens: 2
            })
            .unwrap(),
            r#"{"input_tokens":1,"output_tokens":2}"#
        );
        assert_eq!(
            Usage::default(),
            Usage {
                input_tokens: 0,
                output_tokens: 0
            }
        );
    }
}
