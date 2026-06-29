# Provider — Model API Adapter Subsystem

This is the design record for the provider domain (`src/provider/`): the
`ModelApi` adapter abstraction, the neutral request/response vocabulary, the
config-locked provider selection mechanism, and the `ProviderDescriptor` table.
It is the source of truth for why the system has one trait with two dialect
adapters rather than per-provider clients, and why provider selection is
config-locked for the process lifetime.

Companion documents cover the two dialect implementations
([`docs/provider/dialect/openai_chat_completions.md`](dialect/openai_chat_completions.md),
[`docs/provider/dialect/anthropic_messages.md`](dialect/anthropic_messages.md))
and the three built-in providers
([`docs/provider/kimi_for_coding/kimi_for_coding.md`](kimi_for_coding/kimi_for_coding.md),
[`docs/provider/opencode_go/opencode_go.md`](opencode_go/opencode_go.md),
[`docs/provider/ollama_cloud/ollama_cloud.md`](ollama_cloud/ollama_cloud.md)).
The gateway REST surface for provider and model endpoints is in
[`docs/gateway/route.md`](../gateway/route.md). Read those documents for the
per-dialect and per-provider details; this document covers only the domain core.

## Context

Before this subsystem, the backend had two unrelated model clients driven by the
same `RUBBERDUX_LLM_*` environment variables: `MoonshotClient` (OpenAI
`POST /chat/completions`, at `src/provider/moonshot/`) driving the host
Telegram agent loop, App identity naming, and conversation clustering;
`MessagesClient` (Anthropic `POST /v1/messages`, at
`src/agent/world/model_client.rs`) driving the per-App ECS world runtime. Each
client was hard-wired at its call site, the provider module was named after the
vendor's company rather than the product, the model id was hard-coded, and there
was no way to switch providers from configuration.

The provider refactor unifies those two clients behind one `ModelApi` trait,
renames the module to reflect the product (`kimi_for_coding`), adds OpenCode Go
and Ollama Cloud, and fetches model lists dynamically over REST.

## Architecture

The central design decision is **one `ModelApi` trait, two dialect adapters**.
The two former model-call seams differed only in wire protocol: OpenAI
`POST /chat/completions` versus Anthropic `POST /v1/messages`. A single trait
with `turn()` and `list_models()`, implemented once per dialect, lets every
call site depend on the abstraction. Each provider declares which dialect it
speaks by default, so selecting a provider selects the dialect system-wide —
exactly "lock the entire system to one provider". The diagram below shows the
before and after:

```
Before — two clients, hard-wired per call site:

                     RUBBERDUX_LLM_BASE_URL / _API_KEY / _MODEL
                          (defaults differ per client, no provider key)
                                        │
        ┌───────────────────────────────┴───────────────────────────────┐
        ▼                                                               ▼
┌─────────────────────────┐                    ┌────────────────────────────────┐
│ MoonshotClient          │                    │ MessagesClient                  │
│ OpenAI /chat/completions│                    │ Anthropic /v1/messages          │
│ src/provider/moonshot/  │                    │ src/agent/world/model_client.rs │
└─────────────────────────┘                    └────────────────────────────────┘
        ▲            ▲                                      ▲
  turn_driver   app/identity                         effects.rs / worker.rs

After — one selected provider, one ModelApi, every seam depends on the abstraction:

              RUBBERDUX_LLM_PROVIDER  (kimi-for-coding | opencode-go | ollama-cloud)
              + optional RUBBERDUX_LLM_BASE_URL / _API_KEY / _MODEL  (per-field override)
              + optional RUBBERDUX_LLM_DIALECT  (openai | anthropic)
                                        │
                          provider::selected_from_env()
                                        │  Box<dyn ModelApi>
                  ┌─────────────────────┴─────────────────────┐
                  ▼                                            ▼
   ┌──────────────────────────────┐        ┌──────────────────────────────┐
   │ dialect::OpenAiChatCompletions│        │ dialect::AnthropicMessages    │
   │ turn() + list_models()        │        │ turn() + list_models()        │
   └──────────────┬────────────────┘        └───────────────┬──────────────┘
                  │  (provider descriptor picks ONE dialect)  │
                  └─────────────────────┬─────────────────────┘
                                        │  ModelRequest / ModelResponse (neutral)
        ┌──────────────┬────────────────┼──────────────┬──────────────────┐
        ▼              ▼                ▼              ▼                  ▼
   turn_driver    app/identity    app/merge/      effects.rs         gateway/route.rs
   (host chat)                   clustering      + worker.rs        GET /api/v1/models
                                                 (world)            GET /api/v1/provider
```

**Provider dialect defaults:**
- `kimi-for-coding` → Anthropic Messages (default; carries full Kimi behaviour)
- `opencode-go` → OpenAI Chat Completions
- `ollama-cloud` → OpenAI Chat Completions (tool calling best-effort; warns at selection)

**Known limitation — Kimi quirks under the OpenAI override.** Setting
`RUBBERDUX_LLM_DIALECT=openai` for `kimi-for-coding` does **not** enable the Kimi
OpenAI-only quirks (`$web_search` thinking-disable, `ms://` inline-image upload,
the thinking temperature-lock, `reasoning_content`). `select_from_env`
(`src/provider/mod.rs`) constructs the **generic** `OpenAiChatCompletions`
adapter directly from the resolved dialect, so under the override a plain OpenAI
Chat Completions adapter — with none of those quirks — is what drives the call.
The quirk logic lives only on the concrete legacy `KimiForCodingClient::chat`
(`src/provider/kimi_for_coding/api/chat.rs`), which does NOT implement `ModelApi`
and is therefore off the selection path. Wiring those quirks into the OpenAI
dialect adapter is future work / out of scope. Until then, only the default
Anthropic Messages selection carries full Kimi behaviour.

## Neutral Vocabulary

The neutral request/response types in `src/provider/mod.rs` are the pivot every
call site speaks and every dialect adapter translates from/to. They are modeled
on the types the world already fingerprints — `History`, `ToolSet`,
`ModelConfig` — so that:

- **Switching dialects changes only translation, not the fingerprint basis.**
  The world's `fingerprint_call` hashes `(History, ToolSet, ModelConfig)`, not
  the wire JSON body. Modeling `ModelRequest` on these same concepts means the
  fingerprint is unchanged by a dialect switch: replay stays deterministic and
  no recorded session migrates.

- **Each seam gets everything it needs without coupling to the other.** The host
  seam needs `ToolCall.id` for its entry log; the world seam needs
  `ContentBlock::Reasoning { signature }` for round-tripping Anthropic thinking
  blocks. Both needs are explicit in the neutral union — no seam has to guess
  at what the other requires.

Key types (`src/provider/mod.rs`):

```rust
pub trait ModelApi: Send + Sync {
    fn turn<'a>(&'a self, req: &'a ModelRequest)
        -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>>;
    fn list_models<'a>(&'a self)
        -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>>;
    fn model(&self) -> &str;   // the configured model alias, for logging
}

pub struct ModelRequest { system, messages: Vec<ContentMessage>, tools: Vec<ToolSpec>, sampling: Sampling }
pub enum ContentBlock { Text, Reasoning { text, signature }, ToolUse { id, name, input }, ToolResult { tool_use_id, content, is_error }, Image }
pub struct ModelResponse { blocks, stop_reason, usage, model_id, reasoning: ReasoningPolicy, capabilities }
pub struct ModelInfo { id, owned_by, context_length }
```

`ContentBlock::Reasoning.signature` is opaque: the Anthropic dialect
round-trips it verbatim; the OpenAI dialect emits an empty string for fresh
sessions and never fabricates one. This is correct for replay: a reasoning
block's Anthropic `signature` has no OpenAI equivalent, so switching provider
mid-session is not a supported replay scenario. Deterministic replay re-runs
with the same selected provider that recorded the session.

### Canonical Result Types

`StopReason`, `Usage`, and `ReasoningPolicy` are defined canonically in
`src/provider/mod.rs` (relocated from `src/agent/world/inputs.rs` with their
serde representation preserved so persisted event/snapshot bytes are unchanged).
`agent::world::inputs` re-exports them: one definition, no drift.

## Config-Locked Selection

The entire system locks to **one** configured provider for its process lifetime.
Selection is driven by two functions in `src/provider/mod.rs`:

- **`resolve_from_env() -> Result<ResolvedSelection, Error>`** reads
  `RUBBERDUX_LLM_PROVIDER` (absent → `kimi-for-coding` for back-compat; an
  invalid value is an error, never a silent default), then applies per-field
  overrides: `RUBBERDUX_LLM_DIALECT` (`openai` | `anthropic`),
  `RUBBERDUX_LLM_BASE_URL`, `RUBBERDUX_LLM_API_KEY`, `RUBBERDUX_LLM_MODEL`.
  Each override changes only its own field; unset fields take the provider
  descriptor default. An unknown provider or dialect returns `Err`.

- **`select_from_env() -> Result<(ResolvedSelection, Box<dyn ModelApi>), Error>`**
  calls `resolve_from_env()`, warns on Ollama Cloud (tool-calling caveat), builds
  an HTTP client, and constructs the dialect adapter. Returns both the resolved
  metadata and the adapter so callers that need both (e.g. host startup, which
  populates `GatewayState.provider_meta`) call this once with no second
  resolution.

- **`selected_from_env() -> Result<Box<dyn ModelApi>, Error>`** is the
  single-return-value convenience, routing through `select_from_env`.

**Why config-locked.** Having both seams (host chat and world runtime) call the
same selected adapter is what "lock the entire system to one provider" means: a
stub `ModelApi` injected at startup drives both runtimes uniformly. Per-request
re-selection would allow drift between the host and world seams and would
re-read env vars that may have changed since startup.

## Provider Descriptor Table

`ProviderDescriptor` is a static, heap-free struct carrying per-provider
defaults:

```rust
pub struct ProviderDescriptor {
    pub id: Provider,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
    pub default_dialect: Dialect,
    pub auth: AuthScheme,
}
```

`descriptor(p: Provider)` returns the descriptor for a given provider. The
KimiForCoding descriptor is defined inline in `src/provider/mod.rs`; OpenCode
Go and Ollama Cloud delegate to `opencode_go::descriptor()` and
`ollama_cloud::descriptor()`, which are the canonical homes for those defaults.

No model id or base URL is permanently hard-coded beyond the configurable
default. Defaults are confirmed against live endpoints during end-to-end
verification (see `TODO(verify-live)` annotations in the source).

## Rejected Alternatives

- **Keep two separate clients (MoonshotClient / MessagesClient).** Rejected:
  the two clients had divergent defaults, duplicated HTTP boilerplate, and were
  hard-wired at their call sites. Adding a third provider (OpenCode Go) would
  have required a third client; the trait abstraction scales to N providers
  without new boilerplate.

- **Hard-code the model id per provider.** Rejected: the owner's model evolves.
  Fetching the list live and surfacing it over REST lets operators and clients
  pick or confirm models without a code change. Provider defaults are still
  configurable at runtime via `RUBBERDUX_LLM_MODEL`.

- **Name the module `moonshot` (the vendor's company name).** Rejected: the
  naming convention requires names that survive swapping the vendor library. The
  product name (`kimi_for_coding`) is stable even if the company name or the
  library changes.

- **One trait per provider instead of one trait per dialect.** Rejected: the
  wire protocols (OpenAI Chat Completions, Anthropic Messages) are the real
  source of variation; provider-specific behavior (Kimi's quirks) is a thin
  layer on top. Two dialect adapters shared across providers avoid duplication
  and make the common case (no quirks) free.

- **Dynamic provider switching at runtime.** Rejected: the fingerprint basis,
  replay determinism, and the reasoning-block `signature` guarantee all depend
  on the same provider being in use throughout a session. Runtime switching
  would require session migration, which is out of scope.
