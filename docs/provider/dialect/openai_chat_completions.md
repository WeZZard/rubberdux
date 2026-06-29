# Provider Dialect — OpenAI Chat Completions

This is the design record for the OpenAI Chat Completions dialect adapter
(`src/provider/dialect/openai_chat_completions.rs`). It documents why this
adapter exists as a generic, protocol-only transport shared across OpenAI-shaped
providers, how it translates the neutral vocabulary, and how authentication and
error mapping work.

For the overall provider abstraction and selection mechanism, read
[`docs/provider/provider.md`](../provider.md) first. For provider-specific
behavior built on top of this adapter, read the relevant provider document
([`docs/provider/kimi_for_coding/kimi_for_coding.md`](../kimi_for_coding/kimi_for_coding.md)
for the Kimi OpenAI override,
[`docs/provider/opencode_go/opencode_go.md`](../opencode_go/opencode_go.md),
[`docs/provider/ollama_cloud/ollama_cloud.md`](../ollama_cloud/ollama_cloud.md)).

## Context

The OpenAI Chat Completions wire protocol is the de-facto API surface that many
model providers expose (OpenAI, OpenCode Go, Ollama Cloud, Kimi under the
opt-in override). Rather than implementing a separate client for each
OpenAI-compatible provider, a single generic transport is shared. All
provider-specific behavior (Kimi's `$web_search`, temperature-lock,
`ms://` upload, `reasoning_content`) lives in the provider module, not here.

## What This Adapter Does

`OpenAiChatCompletions` (`src/provider/dialect/openai_chat_completions.rs`)
implements `ModelApi` by:

1. **`turn(&self, req: &ModelRequest) -> Result<ModelResponse, Error>`** —
   translates the neutral `ModelRequest` into a `POST /chat/completions` body
   (messages in OpenAI role/content shape, tools in the OpenAI function-calling
   schema), sends it to `{base_url}/chat/completions` with the configured
   authentication header, and parses the response back into the neutral
   `ModelResponse` (blocks, stop reason, usage, effective `model_id`).

2. **`list_models(&self) -> Result<Vec<ModelInfo>, Error>`** — fetches
   `GET {base_url}/v1/models`, parses the OpenAI `{ "object": "list", "data":
   [...] }` response into `Vec<ModelInfo>`.

3. **`model(&self) -> &str`** — the configured model alias, echoed without a
   network call (used for logging and the `GET /api/v1/provider` response).

## Request Translation

The neutral `ContentBlock` enum maps to OpenAI message content:

| Neutral ContentBlock | OpenAI wire shape |
|---|---|
| `Text { text }` | `{ "type": "text", "text": "…" }` in message content |
| `Reasoning { text, signature }` | dropped entirely — OpenAI Chat Completions has no reasoning request field, so the block is not emitted (never fabricated as text) |
| `ToolUse { id, name, input }` | `tool_calls` array on an assistant message |
| `ToolResult { tool_use_id, content, is_error }` | `tool` role message with matching `tool_call_id` |
| `Image { source: Base64 }` | inline base64 data URL in content parts |
| `Image { source: Url }` | `{ "type": "image_url", "image_url": { "url": "…" } }` |

For sampling, the request body carries only the token limit: `Sampling.max_tokens`
is sent as **both** `max_tokens` (classic OpenAI-compatible endpoints) and
`max_completion_tokens` (newer endpoints). `Sampling.effort` has no OpenAI Chat
Completions request field and is **not** sent — the body has no `reasoning_effort`
field (and no `temperature` field).

## Authentication

The adapter honours two `AuthScheme` values:

- `Bearer` — `Authorization: Bearer <api_key>` (OpenCode Go, Ollama Cloud)
- `AnthropicXApiKey` — `x-api-key: <api_key>` (used when Kimi runs under
  the OpenAI override with Anthropic-style auth)

A `content-type: application/json` header is always sent.

## Error Mapping

A non-2xx HTTP response from the provider is mapped to
`Error::ProviderApi { status: u16, body: String }`. The response body is
collected as text and attached to the error so operators can diagnose rate
limits, authentication failures, and model-not-found responses without
inspecting network traffic. Transport failures (connection refused, timeout,
TLS error) surface as `Error::Http` from the `reqwest` client.

## Why Protocol-Only

The generic adapter contains no provider-specific logic by design. When Kimi
is run under the OpenAI dialect override (via `RUBBERDUX_LLM_DIALECT=openai`),
the KimiForCoding provider applies its quirks — `$web_search` activation,
temperature-lock, `ms://` file-upload for inline images, `reasoning_content`
field — before this adapter sees the request. The adapter itself is unaware of
these transformations.

This separation means:

- A new OpenAI-compatible provider needs only a `ProviderDescriptor`; it gets
  the full `turn()` + `list_models()` implementation for free.
- Adding a Kimi-specific quirk does not touch the generic transport.
- Unit tests for the generic transport use clean neutral requests without any
  provider-specific shaping.

## Rejected Alternatives

- **One adapter per provider (e.g. `OpenAiAdapter`, `KimiOpenAiAdapter`).**
  Rejected: the only difference among OpenAI-shaped providers is the base URL,
  model default, auth scheme, and a thin quirk layer. Duplicating the transport
  for each provider would create identical `turn()` + `list_models()` bodies
  that diverge under maintenance.

- **Make `Reasoning` blocks pass through to OpenAI.** Rejected: OpenAI's Chat
  Completions API has no reasoning round-trip field. Passing a `signature` to
  OpenAI would either be ignored or rejected. The adapter drops the whole
  `Reasoning` block on encode (it emits nothing for it); a fresh OpenAI session
  produces no reasoning block at all.
