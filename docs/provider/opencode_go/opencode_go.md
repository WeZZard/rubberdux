# Provider — OpenCode Go

This is the design record for the OpenCode Go provider
(`src/provider/opencode_go/`): its default dialect, authentication scheme,
configurable defaults, and what distinguishes it from the other built-in
providers.

For the overall provider abstraction and selection, read
[`docs/provider/provider.md`](../provider.md). For the OpenAI Chat Completions
dialect this provider uses, read
[`docs/provider/dialect/openai_chat_completions.md`](../dialect/openai_chat_completions.md).

## What OpenCode Go Is

OpenCode Go is an AI code-assistant service that exposes an OpenAI Chat
Completions-compatible endpoint (`POST /chat/completions`, `GET /v1/models`).
Its `ProviderDescriptor` in `src/provider/opencode_go/mod.rs` declares:

- `default_dialect = Dialect::OpenAiChatCompletions`
- `auth = AuthScheme::Bearer` (standard `Authorization: Bearer <api_key>` header)
- `default_base_url = "https://opencode.ai/zen/v1"` (configurable via
  `RUBBERDUX_LLM_BASE_URL`)
- `default_model = "grok-code"` (configurable via `RUBBERDUX_LLM_MODEL`)

All defaults are unverified placeholders until confirmed by end-to-end
verification (see `TODO(verify-live)` annotations in the source). The base URL
and model id are treated as the best available information from research; the
live endpoint is the authoritative source.

## No Provider-Specific Quirks

OpenCode Go exposes a standard OpenAI Chat Completions surface with no
protocol extensions. The generic `OpenAiChatCompletions` dialect adapter handles
all translation from the neutral `ModelRequest`/`ModelResponse` vocabulary to
the OpenAI wire shape. There is no provider-specific shaping code in
`opencode_go/`.

This is deliberate: the `ProviderDescriptor` table model is designed so that an
OpenAI-compatible provider that needs no quirks can be registered with only a
descriptor — no new Rust types, no new adapter code. OpenCode Go is the
prototypical example of this pattern.

## Configuration

To select OpenCode Go, set `RUBBERDUX_LLM_PROVIDER=opencode-go`. All per-field
overrides apply:

```
RUBBERDUX_LLM_PROVIDER=opencode-go
RUBBERDUX_LLM_API_KEY=<your api key>
RUBBERDUX_LLM_MODEL=<optional override>        # default: grok-code
RUBBERDUX_LLM_BASE_URL=<optional override>     # default: https://opencode.ai/zen/v1
```

`RUBBERDUX_LLM_DIALECT` should not be set for OpenCode Go; the provider's
default is `openai` and overriding to `anthropic` would send the wrong wire
format.

## Rejected Alternatives

- **Hard-code the OpenCode Go base URL and model id.** Rejected: the defaults
  are best-effort and may change as the service evolves. Configurable defaults
  plus live `list_models()` let operators adapt without a code change.
