# App Worker Lifecycle (native subprocess mode)

> Status: started. This document covers the native (out-of-process) App worker
> introduced alongside `src/app/runtime/worker.rs`, the host↔worker RPC frames
> in `src/protocol.rs`, and the `Hello`-based socket routing in `src/host.rs`.
> The supervisor that *spawns* the subprocess, the peer broker behind the peer
> frames, and tombstoning are designed separately and are out of scope here.

## Context

An App (`src/app/mod.rs`) *is* an agent that works while the user observes and
approves. Its agent worker can run two ways:

- **In-process** — `MemorySupervisor` (`src/app/supervisor.rs`) runs one
  `AgentLoop` per App as a tokio task and exposes per-App broadcast channels.
- **Out-of-process (native)** — a separate `rubberduxd` subprocess runs the same
  `AgentLoop` and bridges it to the host over the length-prefixed JSON RPC
  protocol (`src/protocol.rs`). This document is about that subprocess.

The two share the same `AgentLoopBuilder` spawn shape; they differ only in
*where* the loop runs and how its `InputPort`/`OutputPort` are connected. This
keeps the `AppSupervisor` trait contract identical across realizations.

## The worker process

The native worker is selected by `rubberduxd --agent --rpc-host <host:port>
--task-id <app_id> --app-session-dir <dir>` (`src/main.rs`). The presence of
`--app-session-dir` distinguishes app-worker mode from the existing VM-child
agent mode; the other flags are reused unchanged. `--task-id` carries the App
id; `--app-session-dir` is the App's on-disk home directory, under which the
worker roots its `SessionManager` so all session data lands inside the App's
directory rather than the host's home.

Lifecycle (`src/app/runtime/worker.rs`):

1. Connect to the host's RPC listener.
2. **Send `Hello{app_id}` as the first frame.** This is load-bearing: it is how
   the host routes the socket (see *Routing* below).
3. Build an `AgentLoop` via `AgentLoopBuilder` rooted at the App's session.
4. Subscribe the loop's `OutputPort` *before* `run()` so no entry is missed,
   then forward each `EntryNotification` to the host as an
   `AgentToHost::EntryNotification{entry, is_final}` frame.
5. Bridge `HostToAgent::UserMessage` frames into the loop's `InputPort`, and
   honor `HostToAgent::Shutdown`.

## Protocol frames

Added to `AgentToHost` (worker → host):

- `Hello{app_id}` — the mandatory first frame; identifies the App.
- `EntryNotification{entry, is_final}` — one history entry, streamed as appended.
  Mirrors `crate::agent::runtime::port::EntryNotification`.
- `PeerSend{to, payload}`, `PeerList` — peer-messaging frames. Declared here;
  the broker that routes them is a later task.
- `Interaction{interaction}` — a unified-vocabulary interaction
  (`docs/agent/interaction.md`) raised for the user.

Added to `HostToAgent` (host → worker):

- `PeerDeliver{from, payload}`, `PeerListResult{peers}` — counterparts of the
  peer frames.
- `InteractionAnswer{response}` — the answer to a raised interaction.

The existing VM-path frames (`Response`, `SpawnVM`, `ExternalInteraction`,
`UserMessage`, `VMCompleted`, `VMFailed`, `Shutdown`, `InteractionResponse`) are
unchanged; the additions are purely additive so the VM child path keeps working.

## Routing by `Hello` (replacing accept-by-order)

Previously the host accepted RPC connections in order and assumed the next
accept was the VM child it had just launched (`host.rs`, the old
`// TODO: proper connection routing by task_id instead of accept order`). With
multiple App workers connecting concurrently, accept order is no longer a
reliable identity.

`accept_worker` (`src/host.rs`) now classifies each accepted socket by its first
frame:

- A `Hello{app_id}` frame → `AcceptedWorker::App{app_id, stream}`; the socket is
  routed to the matching App. The supervisor that consumes this is wired in a
  later task.
- Any other first frame → `AcceptedWorker::VmChild{first_frame, stream}`; the
  already-read frame is handed back so the VM path processes it without loss.

The VM child path (`run_child_vm`) loops over `accept_worker` until it gets a
VM-child socket, processing the returned first frame before continuing to read.
This preserves existing VM behavior while making the accept decision identity-
based rather than order-based.

## Out of scope

- The `LocalSupervisor` that spawns this subprocess (next task).
- The peer broker / directory behind the peer frames.
- Tombstoning and on-demand restore of native workers.
- The gateway wiring that hands app sockets to the supervisor.
