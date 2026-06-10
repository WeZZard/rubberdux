# App Domain — Supervisor

This is the design record for the App supervisor (`src/app/supervisor.rs`): the
component that owns every App's agent worker and exposes the observable streams
the board renders from. It is the runtime companion to the persistent `App`
domain in [`docs/app/whiteboard-backend.md`](./whiteboard-backend.md) (the
durable cluster entity and its filesystem registry) and to the cross-platform
whiteboard specification in [`docs/apps/whiteboard.md`](../apps/whiteboard.md)
(the conceptual model: whiteboard, app, agent worker, tombstoning). Read those
first.

This document covers only the supervisor seam and its in-process realization. It
does **not** cover the out-of-process `LocalSupervisor`, worker-process mode,
RPC/protocol changes, tombstone persistence beyond status announcement, the
gateway, identity/title-icon generation, auto-merge clustering, peer messaging,
or macOS — each is designed elsewhere.

## Context

The persistent `App` domain gives us a durable cluster entity that survives host
restarts, loaded `Tombstoned` (no worker running). The whiteboard model says an
App *is an agent* the user observes and approves: it must be possible to start a
worker for an App, message it, watch its entries and trajectory, answer the
interactions it raises, and stop it again — all while the board stays live.

The store (`src/app/registry/`) answers *persistence*. It does not run anything.
Something has to own the running worker and turn it into observable streams. That
something is the **supervisor**, and `AppStatus::Active` is its concern alone:
the store always loads Apps `Tombstoned`, so "this App has a live worker" is a
runtime fact the supervisor holds, never a fact read back from disk.

## The Seam

`AppSupervisor` is an async trait. The gateway (designed later) talks to this
trait and to nothing below it: it never learns *where* a worker runs. Today the
only implementation is `MemorySupervisor`, which runs each worker as an
in-process tokio task. Tomorrow a `LocalSupervisor` runs each worker as a child
process, swapped in behind the same trait without the gateway noticing.

This is the central decision: **put the in-process/out-of-process boundary at a
trait, not at a configuration flag inside one supervisor.** A single supervisor
that branched on a "run in subprocess?" flag would interleave two transport
models in one body — channel sends here, RPC frames there — and every method
would carry both paths. A trait keeps each transport model whole and independent;
the in-process one ships first and stays simple, and the subprocess one is added
as a sibling rather than threaded through the first.

### Contract

`AppSupervisor` methods (all `&self`; a supervisor is shared behind an `Arc` and
serializes its own state internally):

- `create_app(request, initial_prompt)` — persist a new App, start its worker,
  deliver the originating task as the first user turn, announce `Created`.
- `list` / `get` — read Apps from the store, overlaying `Active` for any App that
  currently has a live worker.
- `send_message(id, text)` — deliver a user message to a running worker.
- `subscribe_entries(id)` / `subscribe_trajectory(id)` — observe a worker's
  history-entry and trajectory-event streams.
- `pending_interactions(id)` / `respond_to_interaction(id, response)` — read and
  answer the interactions a worker raised (vocabulary from
  [`docs/agent/interaction.md`](../agent/interaction.md)).
- `suspend(id)` / `restore(id)` — stop or start a worker, toggling `Active` ↔
  `Tombstoned`.
- `archive(id)` — suspend, then move the App off the default board.
- `move_app(id, position)` — relocate an App on the board.
- `subscribe_board()` — observe the board's App-lifecycle event stream.

Methods that require a running worker (`send_message`, the `subscribe_*` per-App
streams, the interaction methods) return `Error::App` when the App has no live
worker, so a caller must `restore` first. This keeps "is there a worker?" an
explicit, checked precondition rather than an implicit auto-start side effect.

## Model — MemorySupervisor

`MemorySupervisor` holds the `MoonshotClient`, the `SessionManager`, an
`Arc<dyn AppStore>`, a `Mutex<HashMap<AppId, AppRuntime>>` of live workers, and a
single board broadcast `Sender`. An `AppRuntime` is the handle to one running
worker: its loop `InputPort`, a per-App entry broadcast, a per-App trajectory
broadcast, the list of pending interactions, and a `CancellationToken`.

**Spawning a worker** reuses the pattern in
`src/agent/runtime/subagent.rs::spawn_subagent`: build an `AgentLoop` (via
`AgentLoopBuilder`), subscribe its `OutputPort` **before** driving `run()` so no
entry is missed between spawn and first observation, then forward every
`EntryNotification` onto the App's own entry channel in a small relay task. The
loop is given a `BroadcastTrajectoryRecorder` whose broadcast half is the App's
trajectory channel, so trajectory events fan out to subscribers without extra
plumbing. The loop task runs under `tokio::select!` against the runtime's
`CancellationToken`, so `suspend`/`archive` cleanly stop it.

**Per-App channels, not shared ones.** Each App gets its own entry, trajectory,
and (via the board) lifecycle visibility. The board channel is supervisor-wide
because lifecycle events *are* board-level; entries and trajectory are per-App
because a board observer subscribes to one App's stream at a time.

**Status is overlaid, never persisted as `Active`.** `list`/`get` read the store
(always `Tombstoned`) and flip an App to `Active` iff a runtime exists for it.
`suspend`/`restore`/`archive` announce status transitions on the board channel.
This matches the store invariant that `Active` is never durable.

## Rejected Alternatives

**A single shared in-process loop for all Apps.** One `AgentLoop` multiplexing
every App's conversation would collapse isolation: histories, token budgets,
trajectories, and cancellation would all be entangled, and one App's compaction
or failure would perturb its neighbors. The whiteboard model treats each App as
an independent agent, so each App gets its own loop. The per-App tokio task is
the in-process echo of the per-App subprocess the `LocalSupervisor` will later
spawn — keeping the boundaries identical across both implementations.

**Always-on loops (spawn every App's worker at startup).** The store loads every
App `Tombstoned` precisely so the host does not pay to run every worker on boot.
Eagerly spawning a loop per App would defeat that, scale worker count with total
App count rather than active App count, and contend for the model API and host
resources. Workers are therefore started on demand (`create_app`, `restore`) and
stopped on `suspend`/`archive`; `Active` exists only while a worker is live.

**Auto-start on first message (no explicit `restore`).** Letting `send_message`
silently spawn a worker for a tombstoned App would hide the lifecycle transition
and the cost of starting a worker behind an ordinary message send. Making
`restore` explicit keeps the state machine legible and gives the board a clear
point to show "starting…". `send_message` against a worker-less App is an error,
not a hidden spawn.
