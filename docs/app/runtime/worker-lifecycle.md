# App Worker Lifecycle (native subprocess mode)

> Status: covers the native (out-of-process) App worker
> (`src/app/runtime/worker.rs`), the host↔worker RPC frames in
> `src/protocol.rs`, the `Hello`-based socket routing in `src/host.rs`, and the
> host-side `LocalSupervisor` (`src/app/runtime/local_supervisor.rs`) that spawns,
> connects, pumps, and restarts the subprocess, plus idle tombstoning and
> transparent restore (`src/app/runtime/lifecycle.rs`). The peer broker behind the
> peer frames is designed separately and is out of scope here.

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

## The host side: `LocalSupervisor`

`LocalSupervisor` (`src/app/runtime/local_supervisor.rs`) is the host-side
counterpart of the worker above and the subprocess realization of the
`AppSupervisor` trait (`src/app/supervisor.rs`). Where `MemorySupervisor` runs
each App's `AgentLoop` as an in-process tokio task, `LocalSupervisor` runs it as
a separate OS process — one local `rubberduxd --agent` child per `Active` App
(not a VM) — and bridges that child's RPC duplex into the same per-App broadcast
surface (entries, trajectory) plus the board channel. The two are interchangeable
behind the trait; the gateway never learns which it holds. It does **not** boot a
Tart VM: the child is the host binary itself, located with
`std::env::current_exe()`.

### Binding and the single accept router

On `LocalSupervisor::bind`, the supervisor binds one `TcpListener` on
`127.0.0.1:0` (the OS assigns a free port) and starts a single **accept router**
task. Every worker child dials this one address back. The router calls the shared
`accept_worker` (`src/host.rs`), which classifies each socket by its first frame:

- A `Hello{app_id}` frame → `AcceptedWorker::App`; the router looks up the App's
  **connection inbox** (an `mpsc` channel installed when its worker started) and
  hands the socket to the App's supervision task, which is blocked waiting for
  its (re)connection. Routing is identity-based, not accept-order-based, so many
  workers may connect concurrently.
- Any other first frame → `AcceptedWorker::VmChild`; this supervisor owns only
  native App workers, so the socket is closed.
- A `Hello` with no waiting inbox (a late or stray worker) is dropped.

### Per-App supervision task: spawn → connect → pump → restart

Starting a worker installs the App's connection inbox and spawns a **supervision
task** that owns the child process and the RPC pump across restarts:

1. **Spawn** — `tokio::process::Command` runs `current_exe()` as
   `rubberduxd --agent --rpc-host <addr> --task-id <app_id> --app-session-dir
   <dir>`, with `kill_on_drop(true)`. `--app-session-dir` is the App's on-disk
   home directory (from `AppStore::app_dir`), under which the worker roots its
   `SessionManager`.
2. **Connect** — the task awaits the worker's socket on the connection inbox
   (delivered by the accept router after the worker's `Hello`). If the child
   exits before connecting, that counts as a crash and triggers a restart.
3. **Pump** — a `select!` loop bridges the duplex: inbound
   `AgentToHost::EntryNotification` frames are re-broadcast to the App's per-App
   entry channel (board subscribers read from it); queued outbound
   `HostToAgent` frames (a `send_message` becomes `UserMessage`; an answered
   interaction becomes `InteractionAnswer`) are written to the worker. A
   duplicate `Hello` is ignored (the routing one was already consumed); other
   frames are logged. The loop returns on worker disconnect, child exit (crash),
   or task cancellation. The child is always killed when the iteration returns so
   no process is leaked.
4. **Restart with backoff** — unless the task was cancelled (suspend/archive), a
   crashed or disconnected worker is respawned after an exponential backoff
   starting at 250 ms and capped at 30 s. A worker that stayed up past 10 s is
   treated as healthy, so its backoff resets — an unrelated later crash recovers
   quickly. The **host process stays up** throughout: a crashing worker is
   isolated to its own subprocess and never takes the host down.

### Outbound frames survive reconnects

The board talks to a worker through a `WorkerHandle`
(`src/app/runtime/worker_handle.rs`), which holds the per-App broadcast senders,
the pending-interaction list, the cancellation token, and an `mpsc` outbound
queue. The supervision task drains that queue and writes to the **current**
connection's writer, so frames queued while a crashed worker is restarting are
delivered to the new socket. Suspend/archive cancels the token, which kills the
child, exits the supervision task, and tears down the App's connection inbox.

### Trajectory channel

`WorkerHandle` carries a per-App trajectory broadcast to mirror
`MemorySupervisor`'s surface, but the native RPC protocol does not yet define a
trajectory frame, so that channel stays quiet until such a frame lands. Keeping
it preserves the `AppSupervisor` seam without inventing protocol surface here.

## Idle tombstoning and transparent restore

Tombstoning is an idle-eviction optimization: an `Active` App that has been quiet
long enough has its worker subprocess suspended to free host resources, and the
next message wakes it again so transparently that the caller never observes the
gap. The state machine and the durable resume record live in
`src/app/runtime/lifecycle.rs`; `LocalSupervisor` drives them.

### Lifecycle state machine

`AppLifecycle` (`lifecycle.rs`) is the runtime truth the supervisor owns for each
App — distinct from the on-disk manifest, which always loads `Tombstoned`. It
records the phase (`Active` / `Tombstoned`), the last-activity instant, whether a
turn is in flight, and whether any interaction is awaiting a user answer. Its
mutators are immutable transitions (each returns a new value), matching the
project's functional posture. An App is **idle-evictable** only when it is
`Active`, has no in-flight turn, has no pending interaction, *and* has been quiet
at least the idle window.

These flags are driven from real runtime events, not left inert:

- **Turn started** — `create_app`, `send_message`, and `respond_to_interaction`
  mark the turn in flight (`turn_started`) right after they queue the outbound
  user message / interaction answer to the worker, so the sweeper never evicts an
  App that is still working on a request.
- **Turn finished** — the RPC pump calls `turn_finished` when the worker emits its
  final entry (`EntryNotification{is_final:true}`), refreshing the idle clock from
  the end of the work.
- **Pending interaction** — the pump calls `with_pending_interaction(true)` when an
  `AgentToHost::Interaction` frame arrives (and records the interaction on the
  `WorkerHandle`); `respond_to_interaction` re-syncs the flag from the handle's
  live truth once an answer clears the last pending interaction.

### The idle sweeper

`LocalSupervisor::bind` starts one background sweeper task (no foreground sleep)
that wakes on a fixed cadence (`SWEEP_INTERVAL`, 30 s), collects every
idle-evictable App from the lifecycle map, and tombstones each. The idle window
is read once from `RUBBERDUX_APP_IDLE_SECS` (default 300 s) — named after what it
represents, not after any timer library. The sweep cadence is independent of the
window; it only bounds how long past the window an idle App may linger before
eviction. The sweeper shares the supervisor's `runtimes`, `lifecycles`,
`inboxes`, `store`, and board channel (all `Arc`/clonable), so it evicts through
the same routine `suspend` uses.

### Suspend = persist → shutdown → kill

`suspend` and the sweeper both call the shared `tombstone_app` routine, so manual
and automatic tombstoning follow one sequence:

1. **Persist `resume.json`** — the App's `session.jsonl` already holds the full
   conversation history (the worker's `FilesystemStore` writes it continuously),
   so the resume record carries only the *transient* state that would otherwise
   be lost: the interactions awaiting answers, plus a documented (currently
   empty) slot for peer messages addressed to this App but not yet delivered.
   Peer-message redelivery is a later task; the field exists so the on-disk
   format is stable from the start. The record is written last-writer-wins to
   `{app_dir}/resume.json`.
2. **Cooperative shutdown** — a `HostToAgent::Shutdown` frame is sent so a healthy
   worker can flush and exit on its own. A send failure is ignored (the worker is
   already gone).
3. **Kill on timeout** — after a short grace window (`SHUTDOWN_GRACE`, 2 s) the
   handle's cancellation token fires, killing the child and exiting its
   supervision task (which tears down the App's connection inbox). The App's
   lifecycle record transitions to `Tombstoned` and a `StatusChanged` board event
   is emitted.

### Transparent restore

Every message-path method — `send_message`, `respond_to_interaction`,
`subscribe_entries`, `subscribe_trajectory` — calls `ensure_active` first. If the
App is already `Active` it is a no-op; otherwise the supervisor re-runs the
existing worker spawn path against the App's session directory. The restored
worker rebuilds its history from the durable `session.jsonl`; the supervisor
loads `resume.json` and re-attaches its pending interactions to the new
`WorkerHandle` (`set_pending_interactions`) — carrying the pending-interaction
block forward onto the restored lifecycle record — *before* clearing the file, so
an interaction the worker was awaiting an answer for survives the suspend→restore
gap rather than being discarded. The record is cleared only after it is applied,
so a later tombstone writes a fresh one. A missing or malformed `resume.json` degrades to
an empty record — a restore proceeds on the durable history alone rather than
failing. `restore` (the trait method) is just `ensure_active`. Because restore is
woven into the message path, a caller addressing a tombstoned App sees only the
normal flow: the suspended gap is invisible.

## Out of scope

- The peer broker / directory behind the peer frames. `resume.json` reserves an
  `undelivered_peer_messages` slot for it, but redelivery is a later task.
- The gateway / host-startup wiring that picks `LocalSupervisor` over
  `MemorySupervisor` and hands it the store.
