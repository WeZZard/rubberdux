# Gateway — Single-Agent REST Surface

This is the design record for the single-agent REST surface
(`src/gateway/route.rs`): the HTTP API that exposes conversation entries,
tool calls, trajectory, prompts, provider identity, and the live model list.
It is the route-layer companion to the gateway state additions documented here
and to the multi-App board surface in
[`docs/gateway/apps.md`](apps.md). Read that document for the board routes,
supervisor seam, and WebSocket streams.

This document covers the provider-aware additions — `GET /api/v1/provider`,
`GET /api/v1/models`, and the corresponding `GatewayState` fields
(`selected_provider`, `provider_meta`) — as well as the invariants that govern
all single-agent routes. It does **not** cover the multi-App board REST surface,
WebSocket live streams, or host startup wiring.

## Context

The single-agent gateway exposes one conversation session. Prior to the provider
refactor, each call site built its own model client and the gateway had no
visibility into which provider was in use or which models it offered. The
provider introduction adds two concerns to the gateway layer:

1. **Provider identity.** Clients (operator tooling, the macOS UI) need to know
   which provider is selected, which model it is using, and which wire dialect it
   speaks — without executing another `RUBBERDUX_LLM_*` env resolution on each
   request.

2. **Live model list.** Clients need a model list they can present or consume
   without hard-coding any model id. The selected provider already exposes a
   `list_models()` call; the gateway proxies it over REST.

Both concerns belong to the gateway layer, not to the provider domain: the
provider domain owns the abstraction, the gateway owns the HTTP surface.

## The Surface

All routes are registered in `route::router()`, which is mounted by
`server::run`. The single-agent routes are:

| Method and path | Purpose |
|---|---|
| `GET /api/v1/health` | Liveness check. Returns `{ "status": "ok" }` plus license and source metadata. |
| `GET /api/v1/legal` | Package name, version, license text, and source URL from Cargo metadata. |
| `GET /api/v1/provider` | Provider identity snapshot captured at startup. |
| `GET /api/v1/models` | Live model list proxied from the selected provider. |
| `GET /api/v1/entries` | List conversation entries; supports `?role=` and `?since_id=` filters. |
| `GET /api/v1/entries/{id}` | Fetch one entry. `404` if unknown. |
| `GET /api/v1/tool-calls` | List tool call / tool result pairs from the entry log. |
| `GET /api/v1/trajectory` | Snapshot the session's trajectory events from the JSONL file. |
| `GET /api/v1/prompts/system` | The configured system prompt. |
| `GET /api/v1/prompts/identity` | The configured identity prompt. |
| `GET /api/v1/prompts/soul` | The configured soul prompt. |

The multi-App board routes are additive and are merged in by
`super::apps::router()`. See [`docs/gateway/apps.md`](apps.md).

## Provider Endpoints

### GET /api/v1/provider

Returns a snapshot of the provider identity captured at startup:

```jsonc
{ "provider": "kimi-for-coding", "model": "<configured model>", "dialect": "anthropic-messages" }
```

The values come from `GatewayState.provider_meta: ProviderMeta` — a
`{ provider, model, dialect }` struct populated once from the
`ResolvedSelection` returned by `provider::select_from_env()` at host startup
in `src/host.rs`. The handler (`get_provider`) reads `state.provider_meta`
directly and returns it verbatim, with no per-request env resolution.

**Why no per-request resolution.** `RUBBERDUX_LLM_*` env vars are read once at
startup, the selection is locked for the process lifetime, and re-reading them
per request would be misleading: if the env changed after startup the gateway
would report a provider that no call site actually uses. Capturing the snapshot
at startup keeps the response honest.

### GET /api/v1/models

Proxies the selected provider's live model list into an OpenAI-compatible
envelope:

```jsonc
{ "object": "list", "data": [ { "id": "<model>", "owned_by": "<owner>", "context_length": 262144 } ] }
```

The handler (`list_models`) calls `state.selected_provider.list_models().await`.
On a provider-side error (non-2xx status or transport failure) it returns
`502 Bad Gateway` via `GatewayError::ProviderError`; it never panics.

**Why proxy instead of cache.** The set of models a provider exposes changes
without notice. Caching introduces staleness; proxying always returns the
current list. The latency cost is accepted because model-list reads are
infrequent (operator tooling or UI startup), not hot-path.

## GatewayState Additions

`GatewayState` (`src/gateway/state.rs`) gains two fields for the provider
surface, populated by the `new` and `with_trajectory_tx` constructors:

- **`selected_provider: Arc<dyn ModelApi>`** — the one selected adapter, shared
  with the agent loop and App identity tasks. `list_models()` is called by the
  `GET /api/v1/models` handler. Wired from the single `provider::select_from_env()`
  call at host startup; no per-request re-selection.

- **`provider_meta: ProviderMeta`** — provider identity snapshot (provider id,
  effective model, dialect string). Served verbatim by `GET /api/v1/provider`.

Both fields are required by all `GatewayState` constructors (`new`,
`with_trajectory_tx`, `with_apps`, `attach_apps`) so the provider endpoints are
available regardless of which surface the process serves.

## Error Mapping

`GatewayError` gains one variant for provider failures:

- `ProviderError(message)` → `502 Bad Gateway`, returned when
  `selected_provider.list_models()` fails.

The single-agent entry/prompt handlers return their existing variants
(`EntryNotFound → 404`, generic `500` for other failures).

## Rejected Alternatives

- **Per-request env resolution in `GET /api/v1/provider`.** Rejected: the
  process is locked to one provider; re-reading env vars per request would be
  misleading if the environment changed after startup, and adds unnecessary I/O
  on a read-only handler.

- **Cache the model list in `GatewayState`.** Rejected: the provider's live
  model set changes over time. A stale cache would require an invalidation
  strategy more complex than the simple proxy the owner chose. The latency of
  a live call is acceptable given the access pattern.

- **Serve `GET /api/v1/models` with a `307 Temporary Redirect` to the provider
  URL.** Rejected: it would expose the provider's base URL and API key path
  to clients and bypass the gateway's error handling. Proxying keeps the
  provider URL opaque.
