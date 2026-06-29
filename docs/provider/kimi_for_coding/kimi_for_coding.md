# Provider — KimiForCoding

This is the design record for the KimiForCoding provider
(`src/provider/kimi_for_coding/`): the default provider, its Anthropic Messages
default dialect, the opt-in OpenAI Chat Completions override, the Kimi-specific
quirks (and the known gap that the override does not yet wire them in), and the
name-change rationale.

For the overall provider abstraction and selection, read
[`docs/provider/provider.md`](../provider.md). For the Anthropic dialect this
provider defaults to, read
[`docs/provider/dialect/anthropic_messages.md`](../dialect/anthropic_messages.md).
For the OpenAI dialect used under the override, read
[`docs/provider/dialect/openai_chat_completions.md`](../dialect/openai_chat_completions.md).

## Context

KimiForCoding is the default provider and is the module formerly called
`moonshot` in `src/provider/moonshot/`. The rename reflects the repository's
naming convention: identifiers must be named after the domain concept (the
product), not the vendor's company, so that the name survives swapping the
vendor library. `moonshot` was the company; `kimi_for_coding` is the product
the codebase integrates with.

## Default Dialect: Anthropic Messages

KimiForCoding's `ProviderDescriptor` sets `default_dialect =
Dialect::AnthropicMessages`. This is the primary design decision for this
provider:

**Why Anthropic Messages as the default.** The ECS world runtime's conversation
model is natively Anthropic-shaped: `History` blocks map directly to Anthropic
wire blocks, thinking blocks carry the Anthropic `signature` field, and
`tool_use` / `tool_result` pairs follow the Anthropic protocol. Running over
the Anthropic dialect means the world↔neutral conversion is lossless —
reasoning signatures round-trip, tool-call ids are preserved, and no lossy
translation occurs between the world's canonical block type and the wire body.

Running KimiForCoding over OpenAI Chat Completions (the alternative) would
drop the reasoning `signature` on every assistant turn, lose the structured
`tool_result` shape, and force translation logic that would otherwise be
unnecessary.

**Authentication.** KimiForCoding uses `AuthScheme::AnthropicXApiKey` even
when running over the Anthropic dialect, because Kimi's Anthropic-compatible
endpoint accepts the same `x-api-key` header that Anthropic's own API uses.

## OpenAI Dialect Override

When `RUBBERDUX_LLM_DIALECT=openai` is set, the selection picks the **generic**
OpenAI Chat Completions adapter for the KimiForCoding provider. `select_from_env`
(`src/provider/mod.rs`) constructs `dialect::openai_chat_completions::OpenAiChatCompletions`
directly from the resolved dialect — the same protocol-only transport OpenCode Go
and Ollama Cloud use.

**Known limitation — the override does NOT carry the Kimi quirks.** The
Kimi-specific request shaping below lives only on the concrete legacy
`KimiForCodingClient::chat` (`src/provider/kimi_for_coding/api/chat.rs`). That
type does **not** implement `ModelApi` and is therefore not reachable from the
`select_from_env` selection path. So selecting `kimi-for-coding` with the OpenAI
override yields a plain OpenAI Chat Completions adapter **without** any of these
quirks. Wiring them into the OpenAI dialect adapter (or onto a Kimi-OpenAI
`ModelApi` impl) is future work / out of scope. The Kimi quirks that exist on the
legacy path are:

- **`$web_search`** — Kimi's built-in web search tool, available only over the
  OpenAI protocol. When a tool set includes `$web_search`, the legacy
  `KimiForCodingClient::chat` forces thinking disabled and locks the temperature
  to the model's required value. See `src/provider/kimi_for_coding/api/chat.rs`
  for the request-shaping logic.

- **`ms://` file upload** — Kimi's OpenAI endpoint accepts file references as
  `ms://{file_id}` URIs in place of inline base64 images. The legacy client
  pre-uploads inline base64 images to Kimi's file API before the chat call and
  replaces the data URI with `ms://{file_id}`. On upload failure the original
  data URI is used as a fallback so the call still proceeds.

- **`reasoning_content`** — Kimi's OpenAI endpoint requires the
  `reasoning_content` field on assistant messages to be present and non-empty
  when reasoning was generated. The legacy client ensures this field is populated
  (defaulting to `"(tool call)"` when empty) before the message is sent in a
  follow-up turn.

- **Temperature lock** — the coding model accepts only one temperature per
  thinking state (`0.6` when thinking is disabled, `1.0` otherwise); the legacy
  client forces that value regardless of the requested sampling parameters.

These quirks are implemented in the KimiForCoding module, not in the generic
OpenAI dialect adapter — but, as noted above, the generic adapter that the
override actually selects sees none of them today.

## Why the Quirks Live in the Provider Module

The Kimi-specific behaviors are a provider contract, not a dialect contract:
they depend on which endpoint is being called (Kimi's OpenAI-flavored path),
not on the OpenAI Chat Completions protocol itself. Keeping them in
`kimi_for_coding/` means:

- The generic OpenAI dialect adapter works correctly for OpenCode Go and Ollama
  Cloud without seeing any Kimi logic.
- Adding a new Kimi quirk does not require touching the shared transport.
- Unit tests for the quirks are co-located with the provider, not scattered
  across the dialect.

## Replay and the Dialect Override

Switching KimiForCoding from the Anthropic default to the OpenAI override
mid-session is not a supported replay scenario. A recorded session's reasoning
blocks carry Anthropic `signature` values that have no OpenAI equivalent; replay
with the OpenAI override would drop them and diverge. The operator-facing
guidance is: choose the dialect at startup and keep it for the session's
lifetime. The `fingerprint_call` basis (`History`, `ToolSet`, `ModelConfig`) is
unchanged, so sessions recorded with the Anthropic dialect replay correctly
under the same dialect.

## Rejected Alternatives

- **Keep the module named `moonshot`.** Rejected: the naming convention requires
  names that survive swapping the vendor library. If the company name changes or
  the codebase migrates to a different Kimi-compatible provider, `moonshot` would
  become misleading. `kimi_for_coding` names the product, not the vendor.

- **Default KimiForCoding to the OpenAI dialect.** Rejected: the world runtime's
  native semantics are Anthropic-shaped. Defaulting to OpenAI would require lossy
  translation for every world turn and would drop reasoning signatures by default.
  The OpenAI override is opt-in precisely because it trades that correctness for
  the OpenAI-shaped Kimi endpoint — and, until the quirks are wired into the
  adapter (see *OpenAI Dialect Override*), it does not yet recover the Kimi-only
  features that motivated the OpenAI path in the first place.

- **Implement the Kimi quirks in the generic OpenAI dialect adapter.** Rejected:
  the quirks are specific to Kimi's endpoint; placing them in the generic adapter
  would break OpenCode Go and Ollama Cloud, which use the same dialect without
  these modifications.
