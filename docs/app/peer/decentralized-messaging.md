# Decentralized Peer Messaging

> Status: covers the peer-messaging primitive — the node-addressable
> [`PeerId`](../../../src/app/peer/mod.rs), the dynamic
> [`PeerDirectory`](../../../src/app/peer/directory.rs), the per-App
> [`Mailbox`](../../../src/app/peer/mailbox.rs) (`inbox.jsonl`), the host's
> [`PeerBroker`](../../../src/host.rs) relay, the supervisor integration in
> [`local_supervisor.rs`](../../../src/app/runtime/local_supervisor.rs), the
> worker bridge in [`worker.rs`](../../../src/app/runtime/worker.rs), and the
> agent-facing [`peer_list`/`peer_send`](../../../src/tool/peer_message.rs) tools.

## Context

An App (`src/app/mod.rs`) *is* an agent. Apps do not work in isolation: one App
may need to ask another for help, hand off a result, or coordinate. This is a
**decentralized peer-messaging primitive** — a dynamic directory of available
agents that each agent can query, plus a host-as-relay that brokers messages by a
node-addressable id. The same primitive works locally today and is designed to
federate across machines later without changing any call site.

The primitive answers two questions for an App's agent:

- **"Who can I talk to right now?"** — `peer_list` returns the reachable peers,
  most-recently-used first.
- **"Send this to that App."** — `peer_send` relays an opaque message to a peer by
  its App id; the peer receives it whether it is currently active or offline.

## The model

### Node-addressable `PeerId` (local AND remote capable)

A peer is addressed by a [`PeerId`](../../../src/app/peer/mod.rs): an `AppId` plus
a `NodeId` (the node the App lives on). Every App is `local` today, so the broker
resolves and delivers entirely on this machine. But because the identity already
carries the node, the *identical* broker federates across machines later: a
non-local `PeerId` is forwarded to the node that owns it over a general TCP
transport (the same length-prefixed JSON framing the host already uses in
`src/protocol.rs`). We deliberately did **not** bake in local-only assumptions:
the directory keys on `PeerId`, the broker resolves on `PeerId`, and the inbox
envelope records the sender's `PeerId`. Adding cross-machine forwarding changes no
tool, no worker, and no supervisor call site — only the broker's resolution step
gains a "not local → forward to node" branch.

### The dynamic directory (most-recently-used)

[`PeerDirectory`](../../../src/app/peer/directory.rs) is a *dynamic* registry of
the peers reachable at this moment — never a stale snapshot. It is driven by real
activity:

- An App **registers** when its worker becomes active.
- Every message an App **sends or receives refreshes** its position
  (most-recently-used ordering), so the freshest collaborators surface first in
  `peer_list`.
- An App is **unregistered** when its worker stops.

Recency is a monotonic counter, not a wall clock, so ordering is deterministic and
independent of clock resolution. The directory carries exactly one policy — a
[`may_send`](../../../src/app/peer/directory.rs) gate that forbids an App from
addressing itself. It makes no other routing decision.

### The host is a relay, not an orchestrator (D5)

The host's [`PeerBroker`](../../../src/host.rs) is a **directory + relay — a
switch, not an orchestrator** (decision **D5**). Its whole job is to resolve a
`PeerId` to a connection and forward an opaque message envelope. It makes **no**
routing or coordination decisions beyond a single observable fact: *is this
target's worker live right now?*

- **Live target** → the broker writes a `PeerDeliver` to the target's delivery
  sink and refreshes both peers' recency. It never inspects the payload — the
  payload is a `serde_json::Value` relayed verbatim.
- **Offline target** → the broker queues the envelope to the target's inbox.

Crucially, the broker does **not** decide *whether two Apps should talk*, *how* a
conversation should proceed, or *when* to restore an App. Those would make it an
orchestrator. It is a switch: it connects A to B and forwards bytes. This is why
`relay` returns a `PeerRouteOutcome` (`Delivered` / `Queued` / `Rejected`) rather
than performing side effects itself — the supervisor owns supervision; the broker
owns only resolution and forwarding.

#### Rejected alternative: a coordinating host

We considered a host that owns conversations between Apps — sequencing turns,
deciding which App handles a request, retrying on failure. We rejected it: it
centralizes intelligence the agents already have, couples every interaction to
host logic, and does not federate (a coordinator is a single point that cannot be
split across machines). A dumb switch keeps the network decentralized: the agents
decide what to say and to whom; the host only carries the message.

### The inbox: offline targets stay addressable

A peer addressed while its worker is not running is **not** undeliverable. The
broker appends the envelope to the target App's
[`inbox.jsonl`](../../../src/app/peer/mailbox.rs) (an append-only JSONL log
mirroring the App registry's `members.jsonl`/`merge_log.jsonl` idioms). The inbox
is written into the target's **addressable home** — the directory the App
actually lives in right now, which is the archive directory for a human-archived
App and the live directory otherwise. The store resolves this via
[`AppStore::addressable_home`](../../../src/app/registry/store.rs), so a message
to an archived App lands in the same directory it is restored from rather than in
a fresh live directory; that also keeps an inbox-only directory from shadowing the
archived App's manifest in `AppStore::get`. When the App is next restored,
`LocalSupervisor::ensure_active` drains that same home's inbox in arrival order and
delivers each envelope as a `PeerDeliver`. A delivered peer message
enters the App's loop as an entry with origin
[`EntryOrigin::Peer { app_id }`](../../../src/agent/entry.rs), so it is
attributable to the sending App and distinct from a human user turn.

## Archiving is human-facing, orthogonal to addressability

There are two reasons an App's worker may be offline:

- **Tombstoned** — an *idle-eviction optimization* (see
  `docs/app/runtime/worker-lifecycle.md`): a quiet App's subprocess is suspended
  to free host resources.
- **Human-archived** — a *decluttering choice*: a person moves an App out of the
  default board listing to tidy the whiteboard.

Both are **orthogonal to addressability**. Archiving is a human-facing
organizational concept — it changes what a person sees on the board, not who an
agent can reach. An archived OR tombstoned App:

- is still listed-reachable once active, and is still a valid `peer_send` target
  when offline (the `may_send` gate does **not** consult reachability);
- has messages to it **queued to its inbox and drained on restore** — it does not
  disappear from the agent-to-agent network.

In other words, the peer network and the human-facing board are two different
views. Tombstoning/archiving govern the board view and host resource use;
addressability governs the network view. A message never falls through the gap
between them: it is delivered live, or queued and drained on the next restore.

#### Rejected alternative: archiving removes a peer from the network

We considered treating archive (or tombstone) as "this App left the network", so
peers could no longer address it. We rejected it: it conflates a human's
decluttering action with the agents' connectivity, and it would silently drop
messages to an App a person archived while a peer was mid-conversation with it.
Keeping addressability orthogonal means a human can declutter the board freely
without severing any agent-to-agent link.

## Flow

### `peer_list`

`peer_list` (tool) → worker writes `AgentToHost::PeerList` → the supervision pump
asks `PeerBroker::list_for(self)` → the broker returns the directory's
`addressable_by(self)` (most-recently-used, excluding self) → pump writes
`HostToAgent::PeerListResult{peers}` → the worker fulfills the tool's pending
reply.

### `peer_send`

`peer_send{to, message}` (tool) → worker writes `AgentToHost::PeerSend{to,
payload}` → the supervision pump calls `PeerBroker::relay(from, to, payload)`:

- `Delivered` — the broker wrote `PeerDeliver` to the live target's sink.
- `Queued{wake}` — the target was offline; the broker appended to its
  `inbox.jsonl`. The message drains to the target on its next restore.
- `Rejected` — `from == to`; nothing sent.

The pump makes no routing decision; it hands the envelope to the broker and logs
the outcome.

## Surface boundaries

- The `peer_list`/`peer_send` tools are registered **only in App workers**
  (`AgentLoopBuilder::with_peer_channel`, set by `src/app/runtime/worker.rs`). A
  non-App agent has no peer network and no peer transport, so it gets no peer
  tools.
- The broker lives on the host and is owned by `LocalSupervisor`. Worker children
  reach it only through their RPC duplex — they never touch the broker directly,
  which is what keeps the transport swappable for federation.

## Out of scope (here)

- **Cross-machine forwarding.** The addressing model and broker are
  remote-capable, but the "non-local `PeerId` → forward to node over TCP" branch
  is a later task. Until then every App is `local`.
- **Host-startup wiring** that constructs the `LocalSupervisor` and hands it the
  store (the `host-wiring` task). This document covers the relay machinery the
  supervisor uses; `host.rs::run` integration is completed there.
