# Provider — Ollama Cloud

This is the design record for the Ollama Cloud provider
(`src/provider/ollama_cloud/`): its default dialect, the tool-calling caveat
and why it is not resolved by a native Ollama dialect, and the runtime warning
emitted at selection.

For the overall provider abstraction and selection, read
[`docs/provider/provider.md`](../provider.md). For the OpenAI Chat Completions
dialect this provider uses, read
[`docs/provider/dialect/openai_chat_completions.md`](../dialect/openai_chat_completions.md).

## What Ollama Cloud Is

Ollama Cloud exposes an OpenAI Chat Completions-compatible endpoint over HTTPS.
Its `ProviderDescriptor` in `src/provider/ollama_cloud/mod.rs` declares:

- `default_dialect = Dialect::OpenAiChatCompletions`
- `auth = AuthScheme::Bearer` (standard `Authorization: Bearer <api_key>` header)
- `default_base_url = "https://ollama.com/v1"` (configurable via
  `RUBBERDUX_LLM_BASE_URL`)
- `default_model = "gpt-oss:120b"` (configurable via `RUBBERDUX_LLM_MODEL`)

All defaults are unverified placeholders until confirmed by end-to-end
verification (see `TODO(verify-live)` annotations in the source).

## Tool-Calling Caveat

The per-App world agent issues tool calls — the `set_value` surface tool and
the built-in tool set — via the `ModelApi::turn()` path. When Ollama Cloud is
selected, these calls go through the OpenAI Chat Completions dialect against
Ollama's `/v1` path. Tool calling on Ollama's OpenAI-compatible endpoint is
best-effort and reportedly unreliable: the model may not honor the
`tools` parameter or may produce malformed tool-call responses.

**Consequence.** Selecting Ollama Cloud may cause tool-dependent world agents
to degrade or produce unexpected results. Basic text turns (no tools) are
expected to work correctly.

**At selection**, `provider::select_from_env()` emits a `log::warn!` so
operators are alerted at startup:

```
provider: Ollama Cloud selected — tool calling over the OpenAI Chat Completions
dialect is best-effort/unreliable on Ollama; tool-dependent world agents may
degrade. A native Ollama dialect is out of scope.
```

This warning is intentionally loud: the operator has explicitly opted in to
Ollama Cloud and must understand the trade-off.

## No Native Ollama Dialect

Ollama has a native HTTP API that differs from the OpenAI protocol and may
support tool calling more faithfully. A native Ollama dialect adapter is
**out of scope** for this refactor. The reasons:

- The owner's primary workflow uses KimiForCoding (Anthropic dialect), which
  supports tool use fully. Ollama Cloud is provided for operators who want to
  experiment or run locally without a proprietary API key.
- Implementing a native Ollama dialect requires research into the Ollama wire
  protocol, its tool-call semantics, and its model list format — work that is
  not justified by the owner's current use case.
- The OpenAI-compatible surface is sufficient for text-only sessions and
  provides a working starting point for operators who only need generation.

If a native Ollama dialect becomes necessary, it would live at
`src/provider/dialect/ollama.rs` and be selected by a new `Dialect::Ollama`
variant. The existing `ProviderDescriptor` for Ollama Cloud would update its
`default_dialect` accordingly; no call site would change.

## Configuration

To select Ollama Cloud, set `RUBBERDUX_LLM_PROVIDER=ollama-cloud`. All
per-field overrides apply:

```
RUBBERDUX_LLM_PROVIDER=ollama-cloud
RUBBERDUX_LLM_API_KEY=<your api key>
RUBBERDUX_LLM_MODEL=<optional override>        # default: gpt-oss:120b
RUBBERDUX_LLM_BASE_URL=<optional override>     # default: https://ollama.com/v1
```

## Rejected Alternatives

- **Suppress the tool-calling caveat warning.** Rejected: operators who select
  Ollama Cloud need to know before they encounter a degraded world agent. A
  startup warning costs nothing and surfaces a real constraint.

- **Implement a native Ollama dialect now.** Rejected: the owner's current use
  case does not justify the research and implementation cost. The OpenAI
  compatibility path, with its caveats documented, is the correct scope
  boundary for this refactor.

- **Reject Ollama Cloud selection when tools are in use.** Rejected: the system
  cannot know at selection time which world agents will be started; tool sets
  are per-entity and configured at runtime. A blanket rejection at selection
  would be premature and would block the text-only case that Ollama Cloud
  handles correctly.
