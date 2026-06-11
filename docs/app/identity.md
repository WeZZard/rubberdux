# App Identity Design

## Summary

`derive_identity` takes a free-text task string and returns an `AppIdentity`
(a short title + an `IconSpec`), which the whiteboard uses for labeling and
coloring an App's tile. It never returns an error; the fallback path is always
reachable and fully deterministic.

## Glyph Allowlist Rationale

The allowlist is a curated set of ~24 SF Symbol names that are:

1. Available on macOS 13+ (`symbolVariants` minimum deployment target for the
   whiteboard GUI).
2. Broadly recognizable as task/topic metaphors (not application-specific).
3. Single-word names — making them safe as `symbol` values across the API
   boundary without encoding concerns.

AI-generated illustrated icons were explicitly considered and rejected (decision
D3 in `docs/app/whiteboard-backend.md`): they introduce network latency, cost,
and visual inconsistency across the board. SF Symbol glyphs are instant, free,
scalable, and visually coherent in both light and dark mode.

The LLM is asked to return one symbol from the allowlist as part of a JSON
object. The JSON shape is requested at the prompt level rather than through a
`response_format` constraint: the model (`kimi-for-coding`) is a reasoning model
that spends completion tokens on a visible chain-of-thought before emitting the
answer, and an explicit `response_format: { type: "json_object" }` did not change
that behavior in testing. The load-bearing constraint is instead the
`max_completion_tokens` budget, which must leave room for both the reasoning and
the answer — too small a budget truncates the response (`finish_reason:
"length"`) with empty `content`, forcing the heuristic fallback on every call.
The first balanced `{…}` object is extracted from the response content,
tolerating markdown fences or surrounding prose. If the model returns a symbol
that is not in the allowlist, only the symbol is repaired via the heuristic
fallback; a valid title from the same response is preserved.

## Color Palette Rationale

Ten semantic colors map to common task domains (blue → communication,
green → productivity, orange → creative, …). Colors are stored as `#RRGGBB` hex
strings so they cross the API boundary without committing to a platform color
type. The LLM is asked to pick one color by its hex string; off-palette colors
are replaced via the heuristic fallback.

## Heuristic Fallback

The fallback is triggered in two cases:

- **Partial repair**: the LLM response is valid JSON but one field is invalid.
  Only the invalid field is replaced; the valid fields are kept.
- **Full fallback**: the LLM call fails or the response is malformed JSON.
  The title is derived from the first 40 characters of the task string; the
  symbol and color are derived from a stable FNV-1a hash of the task bytes,
  index-mapped into the allowlist and palette respectively.

FNV-1a was chosen over SHA-2/MD5 because it is dependency-free (no crate
needed), extremely fast, and produces well-distributed values over short strings
— the collision rate for the ~24-entry allowlist does not matter since we only
need stability, not uniqueness.

## Rejected Alternatives

- **AI-generated illustrated icons** — rejected at D3 (whiteboard-backend.md);
  not revisited here.
- **Random assignment** — non-deterministic; the same task would get a different
  icon on every restart.
- **User-provided icons** — deferred to a future preference layer; not in scope
  for the initial implementation.
