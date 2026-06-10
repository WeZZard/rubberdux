# Auto-Merge Clustering Design

## Summary

The clusterer decides whether a new conversation (identified by a short topic
summary) joins an existing App (a cluster of related sessions) or forms a new
App. `POST /apps` consults the clusterer; it merges the new conversation into an
existing App when the topics are sufficiently related, otherwise it creates a
fresh App. The decision is conservative: when the candidates are ambiguous, the
clusterer creates a new App rather than mis-merging unrelated work.

The clusterer is an "active management" surface, not a one-shot gate. Its
`reevaluate` method re-runs the same decision after a cluster's topic has
drifted (e.g. after the first agent turn refines the summary). Scheduling when
`reevaluate` runs is the host-wiring task's concern; this subsystem only
provides the mechanism.

## The `Clusterer` Trait (Mechanism)

The trait exposes two mechanisms, both pure with respect to side effects (the
caller owns persistence and routing):

- `classify(new_summary, candidates) -> ClusterDecision` — decide `Join(index)`
  or `New` for a brand-new conversation against candidate App summaries.
- `reevaluate(member_summary, current_app, candidates) -> ClusterDecision` —
  re-decide membership for a conversation already inside an App, used after the
  topic drifts. Same decision shape; the caller applies the move.

A `ClusterDecision` is `Join { candidate_index }` or `New`. The candidate set is
the App summaries the caller offers; the clusterer never reads the store or the
supervisor directly, so it stays a pure decision function that is trivial to
unit-test.

## A / B / C Trade-offs

### A — LLM classifier (chosen)

A constrained-JSON Moonshot call at **temperature 0** classifies the new summary
against the candidate summaries, returning `{"decision":"join","index":N}` or
`{"decision":"new"}`. Temperature 0 makes the decision as deterministic as the
provider allows, which matters because the same conversation must cluster the
same way on retry. `response_format: { type: "json_object" }` constrains the
output shape, mirroring `docs/app/identity.md`.

Chosen because it captures *semantic* relatedness — two summaries can share a
topic without sharing words ("book a flight" vs. "plan the Tokyo trip"), which a
purely lexical measure misses. It reuses the existing `MoonshotClient` already
wired into the gateway state, so it adds no new dependency or state field.

### B — Embedding similarity (deferred)

Embed each summary into a vector and cluster by cosine similarity. This is the
standard semantic-clustering approach and would be cheaper per call than a chat
completion. It is **deferred** because the `MoonshotClient` exposes no embeddings
endpoint; adopting it would require a new provider surface (and possibly a new
vendor), which is out of scope for this task. The trait shape does not preclude
an embedding-backed implementation later — it is just another `Clusterer`.

### C — Lexical Jaccard (pre-filter + offline fallback)

A token-set Jaccard similarity between the new summary and each candidate
summary. It plays two roles:

1. **Pre-filter** — before the LLM call, candidates are ranked by Jaccard and
   the top *k* are kept (`MAX_LLM_CANDIDATES`), bounding the LLM prompt size and
   cost when the board has many Apps.
2. **Offline fallback** — when the LLM call fails or returns malformed JSON, the
   clusterer falls back to the best Jaccard candidate: it joins only when that
   candidate clears `JACCARD_JOIN_THRESHOLD` (a near-duplicate), otherwise it
   creates a new App. This keeps the clusterer usable without live credentials
   and keeps the gate green in CI.

Lexical similarity alone is too brittle to be the primary mechanism (it misses
synonyms and paraphrase), but it is a cheap, dependency-free, deterministic
bound and a safe default.

## When Clustering Runs

- **On `POST /apps`** — the only point that creates a conversation. The handler
  returns immediately (per the root convention that the chat handler must never
  block); the clustering decision runs on a **background task**. If the decision
  is `Join`, the background task appends the member session to the existing App,
  routes the new conversation's first message into that App's worker via the
  supervisor, records the merge in `merge_log.jsonl`, and bumps `last_active`.
  If the decision is `New`, it keeps the new App as its own and still records the
  decision in that App's `merge_log.jsonl`, so every decision is logged.
- **On drift (later)** — `reevaluate` is invoked after the first turn refines a
  conversation's summary, or on a periodic sweep. Wiring the schedule is the
  host-wiring task's concern; the method exists here so the surface is complete.

## Invariants

- **Never override a `user_locked` App.** A `user_locked` App is excluded from
  the candidate set, so the clusterer can neither merge a new conversation into
  it nor move its members out. The user's pin always wins.
- **Every decision is logged.** Each clusterer decision appends a `MergeRecord`
  to `merge_log.jsonl` (see `src/app/registry/store.rs`), so the cluster's
  history is auditable. The record carries a `MergeDecisionKind`: a `Join` is
  logged against the target App (the App absorbing the conversation); a `New` is
  logged against the new App itself. Logging both kinds means the audit trail
  records why a conversation stayed separate, not only when it merged.
- **A `Join` makes the target most-recently-used.** On a merge, the target App's
  `last_active` is bumped to the current instant and persisted via
  `AppStore::touch_last_active`, so MRU board ordering genuinely reflects the
  merge rather than relying on an incidental worker-side effect of routing the
  message.
- **Conservative default.** Ambiguous or below-threshold decisions create a new
  App rather than risk an incorrect merge.

## Rejected Alternatives Within Scope

- **Blocking the request on the LLM call** — rejected; violates the "chat
  handler must never block" rule. The decision runs as a background task.
- **Merging into `user_locked` Apps with a confirmation prompt** — rejected as
  out of scope; the pin is treated as absolute here.
</content>
</invoke>
