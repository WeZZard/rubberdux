# Gateway — App Board WebSocket Surface

This is the design record for the gateway's multi-App board WebSocket surface
(`src/gateway/apps_stream.rs`): the live streams the whiteboard front end
subscribes to so it never has to poll. It is the streaming companion to the
REST surface in [`docs/gateway/apps.md`](apps.md) (which gives one-shot
snapshots) and rests on the supervisor seam in
[`docs/app/supervisor.md`](../app/supervisor.md) (the `AppSupervisor` trait and
its in-process `MemorySupervisor`). Read those first.

This document covers only the WebSocket surface and the additive gateway-state
field that backs its interaction projection. It does **not** cover the REST
surface, auto-merge clustering, the out-of-process `LocalSupervisor`,
tombstoning, the peer broker, or macOS — each is designed elsewhere.

## Context

The single-agent gateway already streams its one conversation over WebSockets
(`src/gateway/stream.rs`): `ws_entries` and `ws_trajectory` push a broadcast
receiver's items as JSON text frames, and `ws_chat` adds an inbound user-message
path with a `tokio::select!` loop. The board needs the same shape, but
*per App* and *per board*, plus a place to surface the interactions an App
raises.

The supervisor already exposes the streams this surface renders from:

- `subscribe_entries(app_id)` / `subscribe_trajectory(app_id)` — per-App
  broadcast receivers.
- `subscribe_board()` — a board-wide `BoardEvent` receiver
  (`Created` / `StatusChanged` / `Moved` / `Archived`).
- `pending_interactions(app_id)` / `respond_to_interaction(app_id, response)` —
  the interaction snapshot poll and the answer path.

The gateway's job here is narrow: wrap each receiver in the same
broadcast → serialize → `socket.send(Text)` loop the single-agent surface
already uses, with `RecvError::Lagged` (log and resync) and `RecvError::Closed`
(end the stream) handling, and translate the board's domain events into stable,
serde-tagged wire messages.

## Surface

All routes live under `/api/v1/ws`, mounted additively in `server.rs` next to
the existing `/ws/entries|trajectory|chat`:

| Route | Direction | Source |
|-------|-----------|--------|
| `GET /ws/apps/{id}/entries` | outbound | `subscribe_entries(id)` |
| `GET /ws/apps/{id}/trajectory` | outbound | `subscribe_trajectory(id)` |
| `GET /ws/board` | outbound | `subscribe_board()` + interaction projection |
| `GET /ws/apps/{id}/interactions` | bidirectional | interaction projection + `respond_to_interaction` |

### Per-App entry / trajectory streams

Each mirrors its single-agent sibling exactly. The only difference is the
receiver source: instead of `state.entry_tx.subscribe()` the handler resolves
the supervisor with the same `require_supervisor` rule used by the REST surface
and calls `subscribe_entries(id)` / `subscribe_trajectory(id)`. An App with no
active worker yields a supervisor error; the upgrade closes the socket rather
than streaming. The frame shape (`{ "type": "entry", … }` /
`{ "type": "trajectory", … }`) is identical to the single-agent surface so the
front end reuses one decoder.

### Board stream

`subscribe_board()` yields `BoardEvent`s, which the handler projects to stable
wire messages:

- `Created(app)` → `app_created`
- `StatusChanged { … }` / `Moved { … }` → `updated`
- `Archived(id)` → `archived`

The board stream additionally carries a `badge` message announcing the count of
interactions an App is currently awaiting, so the board can render an
attention indicator without opening a per-App interaction socket. See the
interaction projection below for where that signal originates.

### Interaction projection

`AppSupervisor` exposes interactions only as a *snapshot* poll
(`pending_interactions`) and an answer path (`respond_to_interaction`); it has
no live "an interaction was raised" event, and the board's `BoardEvent`
vocabulary carries none. Rather than widen the supervisor trait — which is owned
elsewhere and out of scope here — the gateway owns the live interaction signal
itself.

`GatewayState` gains one additive field, `interaction_tx`, a
`broadcast::Sender<InteractionEvent>` defaulted in every constructor so the
existing single-agent and REST behaviors are unchanged. An `InteractionEvent` is
either `Raised(AgentInteraction)` or `Resolved { app_id, request_id }`. The
board and interaction WebSocket handlers subscribe to this channel; whatever
component comes to own raising interactions (the loop's interaction handler,
designed elsewhere) publishes onto it through the additive
`GatewayState::publish_interaction` helper.

- **Board stream** filters the channel to `Raised` events and emits a `badge`
  carrying the App id and a count, so the board shows where attention is needed.
- **Interactions stream** is per App. On the outbound side it filters the
  channel to that App and emits `interaction_raised` (from `Raised`) and
  `resolved` (from `Resolved`). On the inbound side it accepts a `respond`
  message, calls `respond_to_interaction`, and on success publishes a `Resolved`
  event so every subscriber — including the board's badge — sees the
  interaction clear. The inbound/outbound halves run in one `tokio::select!`
  loop, mirroring `ws_chat`.

This keeps the supervisor trait untouched while giving the front end a single,
coherent, live interaction surface, and it makes the raise → respond → resolved
round-trip drivable end-to-end through the gateway in tests.

## Wire messages

Every message is a serde-tagged struct or enum with a `type` discriminator, so
the front end dispatches on one field:

- Entry: `{ "type": "entry", "entry": …, "is_final": bool }`
- Trajectory: `{ "type": "trajectory", "event": … }`
- Board: `app_created` / `updated` / `archived` / `badge`
- Interaction outbound: `interaction_raised` / `resolved`
- Interaction inbound: `respond` carrying an `InteractionResponse`

## Rejected alternatives

- **Add an interaction event to `BoardEvent` or a `subscribe_interactions`
  method to `AppSupervisor`.** Rejected: the supervisor trait and its impls are
  owned by a separate work stream and are out of scope for this surface. The
  gateway-owned `interaction_tx` projection delivers the same live signal
  without reaching across that boundary.
- **Poll `pending_interactions` on a timer to synthesize raised/resolved
  events.** Rejected: it adds latency and a background timer for a signal the
  raising component can publish directly, and it cannot distinguish a genuine
  resolve from a transient lag.
