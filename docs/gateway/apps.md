# Gateway — App Board REST Surface

This is the design record for the gateway's multi-App board REST surface
(`src/gateway/apps.rs`): the HTTP API the whiteboard front end uses to list,
create, observe, and steer the Apps on the board. It is the gateway-side
companion to the supervisor seam in
[`docs/app/supervisor.md`](../app/supervisor.md) (the `AppSupervisor` trait and
its in-process `MemorySupervisor`) and to the persistent App domain in
[`docs/app/whiteboard-backend.md`](../app/whiteboard-backend.md). Read those
first.

This document covers only the REST surface and the gateway-state additions that
back it. It does **not** cover the WebSocket live-stream surface, auto-merge
clustering on creation, the out-of-process `LocalSupervisor`, host startup
wiring, or macOS — each is designed elsewhere.

## Context

The single-agent gateway (`src/gateway/route.rs`) exposes one conversation:
its entries, tool calls, trajectory, and prompts. The whiteboard introduces
*many* Apps, each its own agent worker with its own history, trajectory, and
pending interactions. The board front end needs a stable HTTP surface to
enumerate Apps, create new ones, move and archive them, send them tasks, read
their state, and answer the interactions they raise.

The supervisor (`AppSupervisor`) already owns every App's worker lifecycle and
exposes the observable streams. The gateway's job here is narrow: translate HTTP
requests into supervisor calls and supervisor results into JSON, without
learning anything about *where* a worker runs.

## The Surface

All routes are additive to the single-agent endpoints; nothing existing is
removed. They are merged into the gateway router by `route::router()` and reach
the network through `server::run`.

| Method & path | Purpose |
|---|---|
| `GET /api/v1/apps` | List Apps on the board (archived omitted). |
| `POST /api/v1/apps` | Create an App from a task; returns `201` immediately. |
| `GET /api/v1/apps/{id}` | Fetch one App. `404` if unknown. |
| `PATCH /api/v1/apps/{id}` | Partial update; today applies a board-position move. |
| `DELETE /api/v1/apps/{id}` | Archive an App; `204`. |
| `POST /api/v1/apps/{id}/restore` | Restore a tombstoned App. |
| `POST /api/v1/apps/{id}/tasks` | Send a task message; returns `202`. |
| `GET /api/v1/apps/{id}/entries` | Snapshot the App's history entries. |
| `GET /api/v1/apps/{id}/trajectory` | Snapshot the App's trajectory events. |
| `GET /api/v1/apps/{id}/interactions` | List pending interactions. |
| `POST /api/v1/apps/{id}/interactions/{request_id}` | Answer an interaction; `204`. |

DTOs (`AppDto`, `IconDto`, `BoardPositionDto`) and request bodies
(`CreateAppBody`, `PatchAppBody`, `SendTaskBody`) are serde `snake_case` and
named for the API boundary, so the wire shape is stable independent of the
internal `App` representation. `AppDto` deliberately omits the App's internal
`member_session_ids` membership log: that is a clustering concern, not a board
client's.

## Non-Blocking Decisions

Two handlers obey the root convention that the chat handler must never block:

- **`POST /apps` returns immediately.** The supervisor persists and starts the
  App with a fast heuristic identity (a neutral symbol and color) so the tile
  renders instantly. Deriving the real title and icon from the task calls the
  LLM, so it is spawned as a background task (`tokio::spawn` over
  `app::identity::derive_identity`). The response carries the created App right
  away; the derived identity reaches the board through the App's streams once
  the identity-update path lands. `derive_identity` is itself infallible, so a
  derivation failure cannot fail the request.
- **`POST /apps/{id}/tasks` returns `202 Accepted`.** The task is handed to the
  worker's agent loop and proceeds asynchronously; the handler does not wait for
  agent progress.

The entries and trajectory reads are one-shot snapshots: they subscribe to the
App's broadcast stream and drain whatever is currently buffered. A live feed is
the WebSocket surface's concern, designed in the next task.

## Identity and Placement

**Collision-resistant ids.** `AppId::now` (`src/app/mod.rs`) mints an id from the
UTC instant at microsecond precision plus a process-local monotonic counter
(`YYYY-MM-DD-HH-MM-SS-ffffff-NNNNNN-UTC`). Two `POST /apps` calls within the same
second therefore receive distinct ids, so concurrent creates never collide on the
filesystem store's `app_dir`. The counter alone guarantees distinctness within a
process even when two mints share a microsecond.

**Sortability tradeoff.** Among ids minted by this code, lexical order matches
creation order (microseconds then counter both increase). Pre-existing
second-granularity ids on disk (`YYYY-MM-DD-HH-MM-SS-UTC`, no suffix) remain
valid directory names and still load. The one edge is the upgrade boundary: a
post-upgrade id that shares a whole second with a pre-upgrade id sorts *before*
it (the `-` of `-UTC` precedes the digits of the suffix). This is accepted as
cosmetic — no correctness path orders Apps by id; board layout is driven by each
App's `{row, column}`, and the id ordering is only a convenience for listings.

**Same-cell placement.** The board does not enforce position uniqueness. Two Apps
may be created at the same `{row, column}`; both persist with distinct ids and
both keep the shared cell. Clients render the overlap stacked. Spreading or
auto-clustering co-located Apps is a client/merge concern, out of scope for the
REST surface. (Pinned by `same_cell_creates_both_persist` in
`tests/integration/gateway/multi_app_board.rs`.)

## Gateway-State Additions

`GatewayState` gains two optional fields behind a new `with_apps` constructor:

- `supervisor: Option<Arc<dyn DynAppSupervisor>>` — the App seam the board
  surface drives.
- `identity_client: Option<Arc<MoonshotClient>>` — the client used for
  background identity derivation.

The existing `new` and `with_trajectory_tx` constructors leave both `None`, so
the single-agent surface compiles and runs unchanged; a board handler called on
a state without a supervisor returns a supervisor error rather than panicking.
One process can therefore serve both surfaces.

## Why an Object-Safe Adapter

The state stores the supervisor behind `Arc<dyn DynAppSupervisor>`, not
`Arc<dyn AppSupervisor>` directly. `AppSupervisor`'s methods return
`impl Future + Send` (return-position `impl Trait` in trait), whose return types
are not nameable in a vtable, so the trait is **not** `dyn`-compatible.
`DynAppSupervisor` (defined in `src/gateway/apps.rs`) restates each method
returning a boxed `BoxFuture`, and a blanket `impl<T: AppSupervisor>` forwards to
it. The gateway can then hold one supervisor behind a trait object regardless of
which concrete supervisor (in-process today, out-of-process later) backs it.
The `with_apps` constructor accepts any `Arc<S: AppSupervisor>` and coerces it
to the object-safe view, so callers never name the adapter.

## Error Mapping

`GatewayError` gains three variants:

- `AppNotFound(id)` → `404`, returned when a handler has proven an App absent
  via `supervisor.get`.
- `InteractionNotFound(request_id)` → `404`, returned when an interaction-answer
  body's `request_id` does not match the path segment.
- `Supervisor(message)` → `500`, the catch-all for a supervisor-layer failure.

Handlers prove App existence with `get` before mutating, so an unknown id is a
clean `404` rather than a generic supervisor error.

## Rejected Alternatives

- **Make `AppSupervisor` itself `dyn`-compatible by boxing in the trait.** That
  would push a gateway-transport concern (trait-object dispatch) into the App
  domain and force every supervisor impl to pay the boxing cost. The adapter
  keeps the seam clean and the cost local to the gateway.
- **Block `POST /apps` until the identity is derived.** Rejected: it violates
  the non-blocking convention and makes tile creation as slow as an LLM round
  trip. The heuristic identity plus background derivation gives an instant tile
  that refines itself.
- **Return full live streams from the entries/trajectory reads.** Rejected here:
  streaming is the WebSocket surface's role. These REST reads are snapshots.
