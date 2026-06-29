# Provider Dialect — Anthropic Messages

This is the design record for the Anthropic Messages dialect adapter
(`src/provider/dialect/anthropic_messages.rs`). It documents why the world
runtime's native wire protocol is elevated to a shared dialect adapter, how the
wire body is built from the neutral vocabulary, how thinking blocks are parsed,
and why the base-URL handling from the former `MessagesClient` is preserved
here.

For the overall provider abstraction, read
[`docs/provider/provider.md`](../provider.md) first. For why KimiForCoding
defaults to this dialect and what changes under the OpenAI override, read
[`docs/provider/kimi_for_coding/kimi_for_coding.md`](../kimi_for_coding/kimi_for_coding.md).

## Context

The ECS world runtime (`src/agent/world/`) has always spoken Anthropic
Messages: its `History` is block-structured to match the Anthropic wire shape,
thinking blocks carry the Anthropic `signature` opaque field, and
`MessageBuilder` serialized directly to the `/v1/messages` body. The former
`MessagesClient` (`src/agent/world/model_client.rs`) was a world-local client
holding this wire logic.

The provider refactor elevates the Anthropic wire logic to a shared dialect
adapter (`AnthropicMessages`) so the world seam, the host seam, and any future
call site all speak the same abstraction. `model_client.rs` is removed; its
wire logic lives here, and the world↔neutral conversion lives in
`agent/world/model_bridge.rs`.

## What This Adapter Does

`AnthropicMessages` (`src/provider/dialect/anthropic_messages.rs`) implements
`ModelApi` by:

1. **`turn(&self, req: &ModelRequest) -> Result<ModelResponse, Error>`** —
   builds a `POST /v1/messages` body from the neutral `ModelRequest`
   (system, messages, tools, sampling), sends it to `{base_url}/v1/messages`
   with Anthropic authentication headers, and parses the response into the
   neutral `ModelResponse` (blocks, stop reason, usage, effective `model_id`,
   reasoning policy).

2. **`list_models(&self) -> Result<Vec<ModelInfo>, Error>`** — fetches
   `GET {base_url}/v1/models` (Anthropic's model list path) and parses it into
   `Vec<ModelInfo>`.

3. **`model(&self) -> &str`** — the configured model alias.

## Request Translation

The neutral `ContentBlock` enum maps to Anthropic wire blocks:

| Neutral ContentBlock | Anthropic wire shape |
|---|---|
| `Text { text }` | `{ "type": "text", "text": "…" }` |
| `Reasoning { text, signature }` | `{ "type": "thinking", "thinking": "…", "signature": "…" }` |
| `ToolUse { id, name, input }` | `{ "type": "tool_use", "id": "…", "name": "…", "input": {…} }` |
| `ToolResult { tool_use_id, content, is_error }` | `{ "type": "tool_result", "tool_use_id": "…", "content": […], "is_error": bool }` |
| `Image { source: Base64 { media_type, data } }` | `{ "type": "image", "source": { "type": "base64", "media_type": "…", "data": "…" } }` |
| `Image { source: Url { url } }` | `{ "type": "image", "source": { "type": "url", "url": "…" } }` |

The request always sends `thinking` as `{ "type": "adaptive" }`, and maps
`Sampling.effort` (`low` / `medium` / `high`) to `output_config.effort`
(serialised lowercase). There is no `thinking.budget_tokens` field. Tools are
omitted entirely when `tools` is empty (byte-identical to a tool-less call).

## Authentication

The adapter honours two `AuthScheme` values and always adds the required
`anthropic-version: 2023-06-01` header:

- `AnthropicXApiKey` — `x-api-key: <api_key>` (KimiForCoding's default auth)
- `Bearer` — `Authorization: Bearer <api_key>` (alt auth for proxied Anthropic
  endpoints)

The `apply_auth` method centralises both paths so the header logic is not
duplicated between `turn` and `list_models`.

## Reasoning Block Round-Trip

Anthropic `thinking` blocks carry an opaque `signature` field that must be
echoed verbatim in subsequent turns for the model to continue reasoning. This
adapter:

- **On parse**: maps `{ "type": "thinking", "thinking": "…", "signature": "…" }`
  to `ContentBlock::Reasoning { text, signature }`.
- **On build**: maps `ContentBlock::Reasoning { text, signature }` back to a
  `{ "type": "thinking", … }` block with the original `signature` value intact.

The `signature` is opaque at the provider level — the adapter neither inspects
nor modifies it. This is why `ContentBlock::Reasoning` carries `signature`
explicitly rather than folding thinking into `Text`: the round-trip guarantee
requires the raw Anthropic value to travel through the neutral vocabulary
unchanged.

## Base-URL Handling

The `turn` call appends `/v1/messages` to the stored `base_url`. When
`base_url` already ends with `/v1` (as some proxy configurations provide), the
resulting path is `{base_url}/messages` — which is the correct target. This
base-URL handling is ported from the former `model_client.rs` so existing
configurations continue to work.

## Error Mapping

A non-2xx HTTP response maps to `Error::ProviderApi { status, body }`. An
unrecognized `stop_reason` value in the response returns `Error::Provider` with
a descriptive message rather than silently defaulting, so operators are alerted
to API surface changes.

## Why Elevated from MessagesClient

The world runtime benefits from the same generic dial-in-once, drive-with-trait
design as the host seam. Keeping the Anthropic wire logic in a world-specific
`MessagesClient` would mean:

- The world and host seams call different code for the same job.
- Adding a new Anthropic-speaking provider (e.g. a Claude endpoint) would
  require another bespoke client.
- Testing the wire logic requires knowing which seam to test.

Elevating to a shared dialect adapter removes all three friction points.

## Rejected Alternatives

- **Keep `MessagesClient` in `src/agent/world/`.** Rejected: it duplicates the
  wire protocol code already in `MoonshotClient` and prevents the world seam
  from benefiting from `ModelApi`'s object-safe dispatch. The neutral vocabulary
  was already the right abstraction; only the mechanical adapter was missing.

- **Inline reasoning round-trip as a string opaque to the neutral layer.**
  Rejected: the neutral `ContentBlock::Reasoning` must carry `signature`
  explicitly because the round-trip is a hard protocol requirement, not an
  optimization. Hiding it behind a string opaque would still require the field
  to travel through — but without type safety.

- **Use the Anthropic streaming SSE surface instead of the blocking POST.**
  Rejected: streaming is a display concern handled by the shell, not a model
  adapter concern. The adapter assembles one complete `ModelResponse` per call;
  the shell streams tokens to the client channel while waiting. Adding streaming
  to the adapter would couple the display path into the domain core.
