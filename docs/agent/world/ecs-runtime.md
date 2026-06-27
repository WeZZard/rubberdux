# Agent World — Event-Sourced ECS Runtime

> Status: DRAFT under hardening. This is the design record for the agent runtime
> rebuilt around the ECS discipline (`src/agent/world/`, planned). It is the
> runtime companion to the App supervisor (`docs/app/supervisor.md`), the worker
> lifecycle (`docs/app/runtime/worker-lifecycle.md`), the interaction vocabulary
> (`docs/agent/interaction.md`), and the whiteboard model
> (`docs/apps/whiteboard.md`). Read those first.

## Context

Today an App's agent is an `AgentLoop` whose state is scattered across struct
fields, live tokio channels, and spawned tasks (`src/agent/runtime/`). That makes
the loop non-serializable and its trajectory non-replayable. This document
specifies a replacement: a **pure, event-sourced ECS World** per App process, so
the agent's full trajectory is deterministically replayable for evaluation, and
its state is a single serializable value.

The differentiator the runtime must serve: the agent **renders and manipulates
the UI directly** across three fluid modes (human-operating, AI-assisted,
AI-driven), and a lead agent may **drive other App processes**.

## Core principles

- **Functional core / imperative shell** (Bernhardt). The **World** is pure and
  deterministic. All I/O and non-determinism (LLM calls, tools, clock, RNG, human
  actions, peer messages) happen in the **shell** as **Effects**, and their
  results re-enter the World as **recorded Inputs**.
- **Event sourcing.** A per-App append-only log is the source of truth. The World
  is rebuilt by replaying the log through the Systems.
- **Two-strata log.** Stratum 1 = **Events** — each wraps one **Logical Input**
  with its `origin` and `edge` (define replay). Stratum 2 = **operational records**:
  **lifecycle events** (traced but behaviorally neutral on replay) AND the **write-ahead
  dispatch-intent** `CommandDispatched` (load-bearing for crash recovery, NOT neutral — see
  *Intent before commitment*). Neither stratum-2 class is folded into the World on replay;
  the dispatch-intent is read only by the resume/recovery path.
- **Record-and-replay for evaluation.** Replay builds a fresh World and feeds the
  logged Logical Inputs through the same Systems tick by tick. The Systems are
  unchanged and **still emit `CallModel`/`RunTool` Commands on replay** — purity
  forbids them from knowing whether they are live. What makes replay "never re-call
  the model or re-run tools" is the **driver**, not a mode flag inside a System: the
  **replay driver discards** every emitted Command while the **live driver dispatches**
  it, and the already-logged Result Inputs stand in for the suppressed Commands. (See
  *Determinism discipline*.) On an UNCHANGED replay the stand-in is total; on a FORK or
  EDIT a recorded result stands in only while the re-emitted request's **fingerprint**
  still matches — past the divergence the branch goes LIVE (see *Correlate by identity*),
  so a stale response is never fed to an edited prefix.
- **Logical time = tick count** (the `clock` ordering counter) — kept strictly
  separate from **observed wall-time**, which is a distinct datum the agent may
  read (the `WallClock` resource), never the ordering axis.
- **Make illegal states unrepresentable** (Minsky) — sum types over loose flags.
- **Provider format: Anthropic Messages** (`POST /v1/messages`); stateless.

```
  WHAT THE USER TOUCHES  — native client renders the agent-described UI
        ▲
  WHAT RUNS              — ECS World (pure); Systems emit Commands; Events carry
        ▲                   origin + edge; "mode" = per-edge fold over event origin
  HOW IT TALKS TO MODEL  — MessageBuilder serializes to Anthropic Messages
```

---

## Data Structure (the schema)

### World

```rust
struct World {
    clock:     Tick,                       // u64; logical ORDERING counter only
                                           // (observed wall-time is Resources.wall, a datum)
    root:      EntityId,                   // the App's primary agent
    entities:  BTreeMap<EntityId, Components>,  // primary + in-process sub-agents; ORDERED by
                                                // EntityId — never a HashMap, whose per-process
                                                // random iteration order is a hidden input that
                                                // would diverge World' on replay (Determinism
                                                // discipline). Every `Map<…>` below is likewise
                                                // a deterministically-ordered map.
    resources: Resources,                  // world-level singletons
}
type Tick = u64;
type EntityId = u32;
```

### Components (per entity = per agent)

```rust
struct Components {
    identity: Identity,
    lineage:  Lineage,
    history:  History,        // canonical block-structured conversation
    budget:   TokenBudget,    // SPEND budget (cumulative → halt) + CONTEXT-window budget
                              // (single-request → compact); two distinct brakes, see the struct
    limits:   EntityLimits,   // per-run loop counter + per-subtree fan-out counter (Boundedness)
    activity: Activity,       // per-entity turn state machine ("what am I doing")
    gate:     EntityGate,     // per-entity run-gate ("am I allowed to run"); distinct from
                              // both Activity (progress) and world-level Resources.gate
    inbox:    Inbox,          // queued Events not yet consumed into a turn; CAPPED, drop-oldest
                              // (Boundedness — a long-Paused app cannot grow it without bound)
    tools:    ToolSet,        // incl. UI-manipulation tools
    raised:   Vec<AgentInteraction>,   // interactions awaiting an answer; CAPPED, reject-newest
                                       // at the cap (Boundedness)
    // Per-entity overrides of the world defaults held in Resources (None ⇒
    // inherit). A world singleton is too singular for these: a sub-agent should be
    // able to run a cheaper/more-restricted model and a tighter autonomy tier than
    // its parent. Resolution rule: entity override else world default.
    model:    Option<ModelConfig>,     // overrides Resources.model
    autonomy: Option<Autonomy>,        // overrides Resources.autonomy
}

enum Identity { Primary, Subagent }
struct Lineage { parent: Option<EntityId>, depth: u8 }
// TWO DISTINCT BUDGETS with different units AND different responses at exhaustion (Boundedness):
//   - SPEND budget (`used`/`limit`): CUMULATIVE token cost across the whole trajectory. Grows
//     monotonically as BudgetSystem folds each `meta.usage` in; on exhaustion the entity HALTS
//     (EntityGate::Halted{BudgetExhausted}) — a hard ceiling on total spend.
//   - CONTEXT window (`context_used`/`context_limit`): the SINGLE-REQUEST input size the next
//     `CallModel` would carry. It does NOT grow monotonically — compaction SHRINKS it. On
//     approaching `context_limit` the entity COMPACTS (summarize older History); it does NOT halt.
// Conflating the two is the classic bug: a context overflow is not a spend overflow.
struct TokenBudget { used: u32, limit: u32, context_used: u32, context_limit: u32 }
// Per-entity boundedness counters (Boundedness). `turns` is the loop guard (model turns since this
// entity's last EXOGENOUS input, vs `Resources.loop_cap`); `spawned` is the per-subtree fan-out
// counter (sub-agents spawned in this subtree, vs `Resources.fanout_cap`).
struct EntityLimits { turns: u32, spawned: u32 }
struct Inbox(Vec<Event>)   // queued Events (envelope carries origin/edge/at); CAPPED at
                           // `Resources.caps.inbox` with DROP-OLDEST backpressure (Boundedness)

// Per-entity run-gate. Orthogonal to Activity (turn progress) and to the world-level
// WorldGate (App-wide run-state): it lets ONE entity halt — e.g. on its own budget
// exhaustion — WITHOUT freezing the whole World, matching the unit of enforcement to
// the per-entity unit of the policy. The halt CONDITION (spend-budget exhaustion) is set by
// BudgetSystem (*Boundedness and backpressure*); this axis only separates the gating.
enum EntityGate { Open, Halted { reason: EntityHalt } }
enum EntityHalt { BudgetExhausted, PerEntityHalt }

// Canonical, Anthropic-block-shaped conversation:
struct History(Vec<Msg>)
struct Msg { role: Role, content: Vec<Block> }
enum Role { User, Assistant }
enum Block {
    Text(String),
    ToolUse   { id: String, name: String, input: Json },
    ToolResult{ tool_use_id: String, content: Vec<Block>, is_error: bool },
    Reasoning { text: String, signature: Opaque },  // echo unchanged
    Image     { source: ImageSource },   // ImageSource is a CONTENT-ADDRESSED blob hash, not inline
                                         // bytes — large blobs are externalized to bound log/snapshot
                                         // size (Boundedness); dedup/consistency is single-source by hash (lens 8)
}

// Per-entity turn state machine:
enum Activity {
    Idle,
    Thinking      { cmd: CmdId },                    // awaiting ModelResponded | ModelFailed
    // A SINGLE assistant turn can MIX local tools, sub-agent delegations, human actions, and
    // peer drives in one batch of tool_use blocks (B1, plus the Codex-r2 unification). Each
    // block becomes ONE `ToolSlot` resolved INDEPENDENTLY by its kind's terminal Input; the
    // entity transitions onward only once EVERY slot is `Done`. The next turn then appends
    // EXACTLY ONE user Msg holding every slot's `result` in ORDINAL (assistant block) order —
    // NOT arrival order — because Anthropic requires every tool_result for one assistant turn
    // in a single following user message. This ONE state replaces the former `UsingTools`,
    // `AwaitingSubagents`, and the tool-driven `AwaitingHuman`: a purely-human turn is just one
    // `Human` slot; a mixed turn carries `Local` + `Child` + `Human` + `Peer` slots together.
    ResolvingToolUses { slots: Vec<ToolSlot> },
    // Awaiting a Compact summarization (Boundedness — context-window). The continuation `CallModel`
    // is DEFERRED until `Compacted` rewrites History; then → Thinking (emit the deferred call). On
    // compaction `ModelFailed` the entity proceeds UN-compacted (best-effort, → Thinking). A wait,
    // so it has a terminating input per the TOTALITY INVARIANT — never a sink.
    Compacting    { cmd: CmdId },                    // compact in flight; → Thinking on Compacted
    // Cancellation is per-state, not only `Thinking` (Totality). Entering `Cancelling` from
    // Thinking/ResolvingToolUses/Compacting records the cmds whose abort acks are still
    // owed; each ack (`InferenceCancelled` / `ToolAborted` / `HumanActionAborted`) — or a real
    // result that races the abort — clears its cmd (a Compact is a model call, so its abort acks
    // as `InferenceCancelled`). Empty ⇒ `Idle`. (`Cancelling` cancels EVERY `Pending` slot of a
    // `ResolvingToolUses` turn — `CancelTool` for `Local`, `AbortHumanAction` for `Human`,
    // `Cancel`-cascade for `Child`; a `Peer` slot's durable send is NOT recallable, so its owed
    // `PeerSendOutcome` is recorded in `awaiting` and absorbed on arrival; a `Done` slot keeps its result.)
    Cancelling    { awaiting: Vec<CmdId> },          // abort acks owed; → Idle when empty
}
// One outstanding tool_use block of a single assistant turn (the Codex-r2 unification of tool +
// sub-agent + human + peer resolution into ordered slots). `ordinal` is the block's position in
// the assistant message — the tool_result ORDERING KEY — NOT the order results arrive. Each slot
// resolves INDEPENDENTLY via its kind's terminal Input: `Local` ← `ToolReturned`, `Child(e)` ←
// `ChildReturned`, `Human` ← `HumanActionDone`, `Peer` ← peer result. When every slot is `Done`,
// the turn assembles EXACTLY ONE user Msg with each slot's `result` in ASCENDING `ordinal` order.
struct ToolSlot {
    tool_use_id: ToolUseId,
    ordinal:     u16,                  // assistant block order; the tool_result ordering key
    kind:        SlotKind,
    state:       SlotState,
    result:      Option<ToolResult>,   // Some once `state == Done` (an `is_error` ToolResult on any
                                       // denial/error/timeout/decline/halt — TOTALITY INVARIANT)
}
enum SlotKind { Local, Child(EntityId), Human, Peer }   // Child carries the spawned child entity
// A `Pending` slot carries the IDENTITY by which its terminal result is mapped back to it (Theme 1a):
// a `Local`/`Human`/`Peer` slot holds `Some(cmd)` — the outstanding shell-dispatched command — so
// `ToolReturned`/`HumanActionDone`/`PeerSendOutcome` resolve it by matching `cmd`. A `Child` slot
// holds `None`: it has no shell-dispatched request, so `ChildReturned` resolves it by IDENTITY
// (`(child, tool_use_id)` per `SlotKind::Child(EntityId)` + `tool_use_id`), not by a `cmd`.
enum SlotState { Pending { cmd: Option<CmdId> }, Done }
// TOTALITY INVARIANT: no path may leave an entity in a waiting state without an eventual
// result. Every denial, error, timeout, refusal, abort, or halt that interrupts an
// outstanding `ToolUse` synthesizes a `ToolResult { is_error: true, content }` into that
// `ToolUse`'s slot so the model can react; every wait has a terminating input.
type ToolUseId = String;   // == ToolUse.id == ToolResult.tool_use_id
type ToolResult = Block;   // specifically a Block::ToolResult

// Per-entity tool registry. Each tool carries an EFFECT CLASSIFICATION because crash
// reconciliation (Intent before commitment) branches on it: an Observational tool is safe to
// re-run blindly on resume; an Effectful tool is re-run ONLY under an idempotency contract
// (`idempotent: true`, re-present the key), else surfaced-as-failed (`is_error` ToolResult)
// rather than silently retried. UI-manipulation tools (`set_value`, …) are Effectful.
struct ToolSet { tools: Map<ToolName, ToolMeta> }
type ToolName = String;
struct ToolMeta { effect: ToolEffect /* , input_schema, description — shape only */ }
enum ToolEffect { Observational, Effectful { idempotent: bool } }
```

### Resources (world singletons)

```rust
struct Resources {
    rng:       Rng,            // deterministic
    wall:      WallClock,      // last OBSERVED wall-time (a datum); NOT the ordering axis
    model:     ModelConfig,    // Anthropic params — WORLD DEFAULT (Components.model overrides)
    autonomy:  Autonomy,       // WORLD DEFAULT (Components.autonomy overrides)
    depth_cap: u8,             // max sub-agent NESTING along any path; single global scalar — see note
    loop_cap:  u32,            // max model turns per run before the loop guard trips (Boundedness)
    fanout_cap: u32,           // max sub-agents per SUBTREE before a spawn is denied (Boundedness;
                               // the per-subtree fan-out budget the Cardinality pass deferred here)
    caps:      Caps,           // queue / context-window / snapshot / restart caps (Boundedness)
    edges:     Map<EdgeId, Edge>,  // counterpart relationships; mode is folded per edge
    gate:      WorldGate,      // App-WIDE run-state (User pause / PolicyHalt) — see two-level note
    ids:       IdAlloc,        // monotonic id allocator — mints `cmd`/`request_id`/`edge_id`/
                               // `timer_id` as PURE functions of World state, never shell-
                               // assigned (Correlate by identity); the SOLE id minter
}

// Boundedness caps — every queue, growing structure, and retry loop carries an explicit brake.
// These are World state (config singletons), so they replay deterministically like everything else.
struct Caps {
    inbox:       QueueCap,     // per-entity Inbox; OVERFLOW = DropOldest (backpressure on a Paused app)
    raised:      QueueCap,     // per-entity RaisedInteractions; OVERFLOW = RejectNewest
    peer_inbox:  QueueCap,     // offline-peer inbox; OVERFLOW = RejectNewest (sender sees `Rejected`)
    mode_window: u32,          // # recent Events the mode-as-projection fold scans, so mode is
                               // O(window) not O(history) (bounds the "recent log")
    snapshot_interval: Tick,   // ticks between World snapshots (restore = nearest snapshot + tail)
    restart:     RestartPolicy,
}
struct QueueCap { limit: u32, overflow: Overflow }
enum Overflow { DropOldest, RejectNewest }   // behavior chosen PER queue above; no queue is unbounded
// Crash/restart backoff + give-up bound (Boundedness). Bounds the resume re-dispatch loop
// (*Intent before commitment*) so a crash-loop cannot spin forever.
struct RestartPolicy {
    base:          Duration,   // first restart delay; DOUBLED each attempt (exponential backoff)
    ceiling:       Duration,   // backoff ceiling
    window:        Duration,   // restart-intensity window
    max_in_window: u32,        // > this many restarts within `window` ⇒ ESCALATE up (give up &
                               // surface to supervisor/human) rather than restart-loop forever
}

// Monotonic id allocator — the SOLE minter of new ids. Each counter is drawn and bumped INSIDE
// the pure reduce step, so replay mints byte-identical ids; ids are NEVER shell-assigned (a
// shell-assigned id is unrecorded nondeterminism that breaks replay correlation — see *Correlate
// by identity*). `effect_id` is deliberately NOT stored here: it is the intra-tick ORDINAL of an
// effectful Command within its emitting step (reset each tick), so `IdempotencyKey = (app_id,
// tick, effect_id)` (Intent before commitment) is unique without a persistent counter.
struct IdAlloc { next_cmd: u32, next_request: u32, next_edge: u32, next_timer: u32 }
type CmdId = u32;               // core-generated, monotonic
type ReqId = u32;               // core-generated, monotonic
type TimerId = u32;             // core-generated, monotonic (minted when a ScheduleTimer is emitted)
struct PeerEnvelopeId(Opaque);  // sender-assigned, stable across the sender's retries — the
                                // cross-PROCESS correlation handle (the per-`cmd` fingerprint
                                // does not cross the process boundary; see *Correlate by identity*)

// Deterministic PRNG — the SOLE source of randomness in the World. The `seed` is a
// recorded hidden input: it enters the log ONCE as the tick-0 `SessionStarted { seed }`
// header (below), so a fresh replay World reseeds IDENTICALLY and every draw reproduces.
// Systems draw only through this Resource; never call an ambient RNG (Determinism discipline).
struct Rng { seed: u64, state: u64 }

// Observed wall-time, separated from the `clock` ordering counter. Holds the last
// wall-time the shell observed; the agent reads it as data (e.g. "what is the date").
// RECORDING MECHANISM (this lens): wall-time is a hidden input that MUST cross a recorded
// boundary. Each `Event` the shell appends carries a `wall: Option<Timestamp>` stamp — the
// real clock read AT record time — and the runtime step folds a present stamp into
// `observed` before the Systems run. Systems therefore read wall-time ONLY from this
// Resource (fed from the log), NEVER the real clock, so replay reproduces every wall read.
// The agent's system-prompt date is one such read and MUST route through `WallClock`
// (see Anthropic model-call mapping), not `now()`.
struct WallClock { observed: Option<Timestamp> }  // None until first observed
type Timestamp = i64;   // shape only; exact encoding fixed by a later pass

type EdgeId = u32;
// An Edge is a FIRST-CLASS relationship between this World (or an entity in it) and a
// counterpart. Mode is a pure fold over the origins of recent Events on a given edge,
// so a lead agent can be Assisted on its human edge and Driven on its app edges at once.
struct Edge { id: EdgeId, counterpart: Counterpart }
enum Counterpart {
    Human,          // a human-facing surface (the native client UI)
    Peer(PeerId),   // a peer App process
}
// DETERMINISTIC edge binding (Theme 4a). `edge_for(Counterpart) -> EdgeId` resolves the LOCAL edge
// for a durable `Counterpart` — keyed by the `Counterpart` itself, never chosen out-of-band — so a
// receiver binds an inbound `DriveRequested`/`PeerDelivered` (`Counterpart::Peer(from)`) to a stable
// local edge in the LOG rather than ad hoc. The FIRST time an edge is created for a `Counterpart`,
// the binding is logged as an `EdgeBound { edge, counterpart }` Input so replay reproduces the same
// `EdgeId`; thereafter `edge_for` returns the bound edge from `Resources.edges`.
// fn edge_for(c: Counterpart) -> EdgeId  // pure lookup over Resources.edges; mints + logs EdgeBound on first use

struct ModelConfig { model: String, max_tokens: u32, effort: Effort }
enum Autonomy { AskEverything, GateTier(Tier), RunFree }
enum Tier { FreeRun, FlagReversible, BlockIrreversible }
enum WorldGate { Open, Paused { holds: Vec<PauseHold> } }  // `holds` non-empty while Paused
struct PauseHold { reason: PauseReason, since: Tick }
enum PauseReason { User, PolicyHalt(GuardrailTrip) }
```

**Cardinality notes (this lens).**

- **Per-entity overrides.** `ModelConfig` and `Autonomy` are world *defaults* in
  `Resources` but optional *overrides* in `Components` (`model`, `autonomy`).
  Resolution is **entity override else world default**, so a sub-agent can run a
  cheaper/more-restricted model and a tighter autonomy tier than its parent.
- **`depth_cap` stays a single global scalar.** It bounds *nesting depth* along any
  root→leaf path (checked against per-entity `Lineage.depth`), which is genuinely
  one global invariant — correct cardinality. A per-subtree *fan-out/count* budget
  (total children spawned, not nesting depth) is a different quantity and a
  boundedness concern, not a cardinality one; it is deliberately left out here.
- **`WorldGate::Paused` holds a set, not one reason.** A user pause and a policy
  halt (and distinct guardrail trips) are independent sources that can coexist, so
  a single `reason` is too singular; `holds` is non-empty while Paused and the gate
  returns to `Open` only when ALL holds clear. (The exact per-hold clearing
  semantics of `Resume`/`PolicyHalt` belong to the Gate/totality pass.)

**Separation-of-concerns notes (this lens).**

- **`origin` + `edge` are structural, not inferred.** Every Logical Input enters the
  log inside an `Event { origin, edge, at, input }`. Mode is then a PURE FOLD over an
  edge's recent events (`mode(edge) = classify(...)`), so the concurrent case (Assisted
  on the human edge, Driven on app edges, simultaneously) is *computable* rather than
  merely asserted. `Edge`/`Counterpart` model the relationship as a first-class concept.
- **Ordering vs. observed wall-time are different concerns.** `clock: Tick` is the
  ordering counter; `Resources.wall: WallClock` is observed wall-time as data. They no
  longer share one axis. (The recording mechanism — the `Event.wall` stamp folded into
  `WallClock` — is now defined by the Hidden-Inputs pass; see that struct's comment.)
- **Answer vs. call-metadata are different concerns.** `ModelResponded` carries
  `blocks` (the answer) AND `meta: ModelMeta` (usage, the model id actually used,
  `stop_reason`), so a System can branch on metadata without parsing content. (Wiring
  `stop_reason` into transitions is the Totality pass — only the *separation* is done.)
- **Two-level gating (granularity match).** App-wide run-state is `Resources.gate:
  WorldGate` (User pause / PolicyHalt), owned by GateSystem. Per-entity run-state is
  `Components.gate: EntityGate`, so one sub-agent's halt does not freeze the World. A
  transition fires only when BOTH gates are `Open`. The world gate owns App-wide concerns;
  the entity gate owns per-entity concerns (e.g. that entity's budget exhaustion). The
  halt *conditions* (budget enforcement) belong to the Boundedness/Totality pass.

### Inputs (Stratum 1 — logical) and Lifecycle events (Stratum 2)

```rust
// Every Logical Input is logged inside an Event envelope. The envelope makes the
// two facts the mode projection needs STRUCTURAL instead of inferred: WHO produced
// the input (`origin`) and WHICH counterpart relationship it pertains to (`edge`).
// Stratum 1 is therefore a log of Events, not bare LogicalInputs. Effect results
// also carry an edge (the relationship the turn is serving), so agent-driven work
// on an app edge folds to Driven while human work on the human edge folds to Operating.
struct Event {
    origin: Origin,        // who produced this input
    edge:   EdgeId,        // the counterpart relationship it pertains to
    at:     Tick,          // logical time recorded (ORDERING axis)
    wall:   Option<Timestamp>,  // OBSERVED wall-time the shell read when it appended this
                                // Event (the recorded hidden-input boundary for wall-time);
                                // folded into Resources.wall before Systems run. None when
                                // the shell took no clock reading for this Event.
    input:  LogicalInput,  // the payload
}
enum Origin { Human, Agent, System, Peer }

// Stratum 1 is NOT homogeneous — it partitions into two CORRELATION CLASSES, and that split is
// the basis of content-addressed replay (see *Correlate by identity*). EXOGENOUS inputs are the
// trajectory's FREE VARIABLES: they have no originating Command, so an edit/rewind carries them
// across VERBATIM and they are never fingerprinted. DERIVED inputs are an EFFECT-RESULT CACHE:
// each answers a specific dispatched Command, carries that request's `fingerprint` and the
// `entity` it routes to, and is reused on replay ONLY while the re-emitted request re-hashes to
// the same `fingerprint` (else it is stale and the branch falls to live execution).
enum LogicalInput {
    // session header (tick-0). Recorded ONCE when the log is opened so a fresh replay World
    // reseeds `Rng` IDENTICALLY (the seed is a hidden input that MUST cross a recorded
    // boundary). The `wall` of this Event also primes `WallClock.observed` before any turn.
    // EXOGENOUS (a free variable).
    SessionStarted   { seed: u64 },

    // === EXOGENOUS — free variables; preserved verbatim across edit/rewind; never fingerprinted ===
    // user intents (all normalize here)
    UserMessage      { to: EntityId, text: String },
    Pause            { reason: PauseReason },
    Resume,                                            // clears every `User` hold (Gate)
    ClearPolicyHalt  { trip: GuardrailTrip, authority: Authority },  // clears ONE PolicyHalt hold
    // Per-entity analog of `ClearPolicyHalt` (human/system origin): reopens ONE entity's
    // `EntityGate::Halted` (e.g. a budget halt) → `Open`. Without it a budget-halted entity has no
    // clearing Input and is a permanent sink (Totality); BudgetSystem owns the reopen. Authority
    // gated; enforcement deferred.
    ClearEntityHalt  { entity: EntityId, authority: Authority },     // reopens one EntityGate::Halted
    Cancel           { entity: EntityId },
    InteractionAnswer{ request_id: ReqId, answer: InteractionResponse },  // correlated to its
                                                                          // RaiseInteraction by the
                                                                          // stable `request_id`
    SetAutonomy      { policy: Autonomy },
    // RECEIVER EDGE BINDING (Theme 4a). The receiver's binding of a `Counterpart` to a LOCAL
    // `EdgeId` is a LOGGED fact, not an out-of-band choice: the deterministic `edge_for(Counterpart)`
    // rule (see `Edge`/`Counterpart`) keys an edge by the durable `Counterpart`, and this Input is
    // logged the FIRST time that edge is created so the binding is replayable. Inbound peer appends
    // (`DriveRequested`/`PeerDelivered`) bind to `edge_for(Counterpart::Peer(from))`. EXOGENOUS
    // (derived-on-receiver from the inbound envelope; never fingerprinted).
    EdgeBound        { edge: EdgeId, counterpart: Counterpart },
    // inbound peer traffic (origin Peer) — free variables from OUTSIDE this World, no originating
    // Command here, so de-duped by the sender's stable `envelope` rather than by arrival order.
    // Binds to `edge_for(Counterpart::Peer(from))` (Theme 4a).
    PeerDelivered    { from: PeerId, envelope: PeerEnvelopeId, payload: Json },
    // cross-World driving (B10). Inbound: another App's lead drives THIS app; the shell
    // executes `drive` against this client and records it here (origin Peer) with the
    // sender's authorization, so A acting inside B is auditable in B's own log. Correlated to
    // the sender's outbound `SendPeer` by `envelope` (the per-`cmd` fingerprint does not cross
    // the process boundary), and bound to `edge_for(Counterpart::Peer(from))` (Theme 4a).
    DriveRequested   { from: PeerId, envelope: PeerEnvelopeId, drive: DriveCommand, auth: Authorization },
    // direct HUMAN UI manipulation — B8. This stratum-1 input is the HUMAN-ORIGIN case ONLY
    // (origin Human, on the human `edge`): a human grabbing the wheel — a primary EXOGENOUS free
    // variable that feeds mode-as-projection ⇒ Operating, with no originating effect, its own
    // canonical record. AGENT UI writes are NOT a `SurfaceMutated` input: an agent write is the
    // Effectful `set_value` `RunTool`, recorded ONCE as `ToolUse` + `CommandDispatched` +
    // `ToolReturned` (the single UI-write commit record, inheriting the write-ahead `key` so a
    // resume re-dispatch does NOT double-apply and world↔screen cannot desync). The agent-origin
    // surface change used for the surface view + mode-as-projection is a PROJECTION derived from
    // that `ToolReturned` (folded once by `cmd`), NOT an independent logged input — see *Single
    // source of truth*, lens 8. So `SurfaceMutated`-as-input is purely EXOGENOUS/human-origin.
    // ECHO DEDUP (Theme 1e): a native UI change signal from the OS carries `cause: Cause`. The
    // shell logs ONLY a `cause = Human` signal as this stratum-1 `SurfaceMutated`; a `Command`- or
    // `Peer`-caused signal is the screen's own echo of an already-logged `ToolReturned`/
    // `DriveRequested` and is DROPPED (deduped) — so an agent `set_value` followed by the UI's own
    // change event is NOT double-counted and does not flap the mode. `op` is a payload-bearing
    // `SurfaceOp` (Theme 2a) carrying its `surface`/`element`/`value`/optional `base_version`.
    SurfaceMutated   { op: SurfaceOp },

    // UI PERCEPTION as a first-class input (Theme 2b). Records the macOS UI state the agent
    // perceives at a tick — closing the "UI state is a hidden input" replay hole: a UI/`set_value`
    // tool's `Fingerprint` MUST fold in the relevant `surface_version`/`ax_digest` (round-2
    // "Fingerprint vs external state": UI tools are External-state, stance (a)), so a re-emitted UI
    // request diverges exactly when the perceived UI state changed. EXOGENOUS (a free variable,
    // origin Human/System on the relevant `edge`); never fingerprinted itself.
    SurfaceObserved  { surface: SurfaceId, version: SurfaceVersion, ax_digest: Hash,
                       focus: Option<ElementId>, selection: Option<Selection>,
                       viewport: Viewport, window: WindowState, cursor: Option<Point> },

    // === DERIVED — effect-result cache; each fingerprinted result carries the request `fingerprint`
    //     AND the routing `entity` (inherited from `CommandDispatched.ctx` — Theme 1b), reused on
    //     replay only on fingerprint match (else stale → live). The IDENTITY-correlated, deliberately
    //     NON-fingerprinted exceptions are `ChildReturned` (child entity + tool_use_id) and the abort
    //     acks `ToolAborted`/`HumanActionAborted` (`cmd`-keyed) — see their comments. ===
    // `blocks` (the answer) and `meta` (call metadata) are separate concerns. The agent/World
    // consumes the ASSEMBLED CANONICAL `blocks` ONLY; raw streamed tokens are a SHELL concern and
    // are NEVER an input to a System (streaming is display-only) — there is EXACTLY ONE
    // `ModelResponded` per inference, the sole System-visible result. `entity` is echoed
    // from the originating Command so the result is SELF-ROUTING (no positional scan); a late
    // result for an already-finalized `cmd` is matched and idempotently dropped, never misrouted.
    ModelResponded   { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, blocks: Vec<Block>, meta: ModelMeta },
    ModelFailed      { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, error: ModelError },  // model call did not return blocks
    InferenceCancelled{ cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, partial: Option<String>, reason: CancelReason },  // Requested ack OR Crash (resume-synthesized)
    ToolReturned     { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, result: Vec<Block> },  // a tool ERROR rides as `is_error` in the ToolResult
    // a child sub-agent's COMPLETION, made an explicit correlated input (Codex-r2) so it resolves
    // the parent's `Child` slot like any other result — a logged, correlated event, NOT an implicit
    // in-World fold whose timing depends on System order. Correlated by IDENTITY (`child` entity +
    // `tool_use_id`), not by a request fingerprint: the child has no shell-dispatched request to
    // hash; on replay it is regenerated deterministically when the child entity reaches its EndTurn
    // (the child's own log reproduces it). `result` is the child's final answer (or an `is_error`
    // ToolResult on a child terminal failure — see SupervisionSystem, Totality).
    ChildReturned    { parent: EntityId, child: EntityId, tool_use_id: ToolUseId, result: ToolResult },
    ToolAborted      { cmd: CmdId, entity: EntityId },         // ack of CancelTool (no fingerprint: an abort has no request payload to hash)
    HumanActionDone  { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, result: HumanResult },  // Provided | Declined | Timeout
    HumanActionAborted{ cmd: CmdId, entity: EntityId },        // ack of AbortHumanAction (no fingerprint)
    Timer            { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, id: TimerId },  // result of ScheduleTimer; `id` is its own stable key too
    // Outbound bookkeeping: the SENDER records the delivery outcome of its own `SendPeer`
    // (correlated by `cmd`) so its replay is stable regardless of how the peer fared.
    PeerSendOutcome  { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, to: PeerId, outcome: DeliveryOutcome },
    // result of Compact (Boundedness — context-window): a single `summary` Msg that REPLACES the
    // oldest `replaced` History Msgs, shrinking the next request's context window. DERIVED — carries
    // the request `fingerprint`; reused on replay only on match.
    Compacted        { cmd: CmdId, entity: EntityId, fingerprint: Fingerprint, summary: Vec<Block>, replaced: u32 },
}
// (Steering is NOT an Input — it is UserMessage + optional Cancel.)

// Call metadata, kept distinct from the answer (`blocks`). A System branches on
// `meta` (e.g. usage/stop_reason) without parsing content. Shape only — wiring
// `stop_reason` into Activity transitions is the Totality pass, not this one.
// HIDDEN-INPUT RULE (this lens): the routing/capability/reasoning facts a turn depended
// on are RECORDED here per call, so replay reproduces the SAME branch from the log and a
// System never re-resolves them from a live table. Specifically:
//   - `model_id` is the EFFECTIVE model actually used (not the requested alias);
//   - `capabilities` is the capability metadata in force for THIS call;
//   - `reasoning` is the reasoning round-trip policy actually applied to THIS call.
// (These per-call records ARE the single source of truth on replay vs. any live capability
// table — the table is consulted only to PRODUCE the record; see *Single source of truth*, lens 8.)
struct ModelMeta {
    usage:        Usage,
    model_id:     String,          // effective model id, recorded — never re-resolved on replay
    stop_reason:  StopReason,
    capabilities: Capabilities,    // capability metadata that governed THIS call
    reasoning:    ReasoningPolicy, // reasoning round-trip actually applied to THIS call
}
struct Usage { input_tokens: u32, output_tokens: u32 }
enum StopReason { EndTurn, ToolUse, MaxTokens, Refusal, PauseTurn }
struct Capabilities(Opaque);   // shape only; exact fields fixed by a later pass
// How reasoning blocks were round-tripped for this call: echoed back verbatim, dropped,
// or required-to-echo. Recorded so replay reconstructs the SAME History, deterministically.
enum ReasoningPolicy { Echo, Drop, MustEcho }

// --- Totality support types (shape only; encodings fixed by later passes) ---
// Model-call failure, distinct from a successful non-`EndTurn` stop_reason: the call yielded
// no blocks at all (HTTP/transport/overload). Drives the `Thinking → Idle` failure edge.
enum ModelError { Http(u16), Timeout, Overloaded, Transport(String) }
// Why an inference was cancelled. `Requested` is the ack of an explicit `CancelInference`;
// `Crash` is SYNTHESIZED on resume when a `CallModel` dispatch-intent has no result Input —
// turning the crash's lost-effect consequence into a real stratum-1 Input (Intent before
// commitment) so budget/Autonomy account for it and replay reproduces it deterministically.
enum CancelReason { Requested, Crash }
// Human-action outcomes that resolve a `Human` slot of a `ResolvingToolUses` turn (every such
// slot backs a tool_use block, so each outcome fills the slot's `result`): `Provided` → the
// answer becomes the slot's `ToolResult`; `Declined`/`Timeout` → an `is_error` `ToolResult` into
// the slot so the model can react. The turn continues once every slot is `Done`.
enum HumanResult { Provided(Json), Declined, Timeout }
// Cross-World drive payload (B10): payload-bearing surface ops (Theme 2a — each names its own
// `surface`/`element`/`value`/`base_version`) the target's shell applies to its own client, plus an
// optional prompt routed to the target agent's Inbox. Carried in `PeerPayload::Drive` over `SendPeer`.
struct DriveCommand { surface_ops: Vec<SurfaceOp>, prompt: Option<String> }
enum DeliveryOutcome { Delivered, Queued, Rejected }   // sender-side result of SendPeer
// Authority/authorization are SHAPE ONLY here. The totality requirement is that the
// transition EXISTS and is gated by a token; WHO may issue it and how it is verified is an
// enforcement/guardrail concern left to a later pass.
struct Authority(Opaque);                              // clears a PolicyHalt hold
struct Authorization { from: PeerId, token: Opaque }   // authorizes A to drive inside B
type SurfaceId = u32;
type ElementId = u32;
// PAYLOAD-BEARING surface op (Theme 2a) — replaces the bare `Render|SetValue|Click|Navigate`
// markers so a lead agent can NAME exactly what it manipulates (locally via the `set_value`/UI
// `RunTool`, and cross-World via `DriveCommand.surface_ops`). Each variant carries an optional
// `base_version: Option<SurfaceVersion>` PRECONDITION (optimistic concurrency, Theme 2c): an op
// whose `base_version` no longer matches the current `SurfaceVersion` is REJECTED with an
// `is_error` result rather than clobbering a changed surface.
enum SurfaceOp {
    SetValue { surface: SurfaceId, element: ElementId, value: Value,         base_version: Option<SurfaceVersion> },
    Click    { surface: SurfaceId, element: ElementId, point: Option<Point>, base_version: Option<SurfaceVersion> },
    Navigate { surface: SurfaceId, route: Route,                             base_version: Option<SurfaceVersion> },
    Render   { surface: SurfaceId, component: ComponentSpec,                 base_version: Option<SurfaceVersion> },
}
// Per-surface / per-element optimistic-concurrency version (Theme 2c). Bumped on every applied
// surface change; an op's `base_version` is checked against it before apply.
type SurfaceVersion = u64;
// The cause the OS attaches to a native UI change signal (Theme 1e) — the basis of echo dedup.
// Only `Human` is logged as a stratum-1 `SurfaceMutated`; `Command`/`Peer` signals are echoes of
// an already-logged `ToolReturned`/`DriveRequested` and are dropped.
enum Cause { Human, Command { cmd: CmdId, key: IdempotencyKey }, Peer { envelope: PeerEnvelopeId } }
// Shape-only perception/manipulation support types (encodings fixed by a later pass).
type Value = Json;
type Hash = Opaque;
type Point = (u32, u32);
struct Route(Opaque);
struct ComponentSpec(Opaque);
struct Selection(Opaque);
struct Viewport(Opaque);
struct WindowState(Opaque);

enum LifecycleEvent {
    // Behaviorally NEUTRAL on replay (trace only). A crash/respawn does not itself mutate the
    // World: the crash's behavioral consequence is realized as a SEPARATE stratum-1 Input — see
    // *Intent before commitment* (the synthesized `InferenceCancelled{reason:Crash}` or
    // `is_error` ToolResult). These four stay neutral; the amendment is that the crash's EFFECT
    // is no longer "nowhere" — it lives in a stratum-1 Input, not in this stratum-2 record.
    Tombstoned     { at: Tick },
    Restored       { at: Tick },
    WorkerCrashed  { at: Tick, err: String },
    WorkerRespawned{ at: Tick },
    // Observable drop notice (Theme 5d) — the concrete type behind the round-3 `Inbox` DropOldest
    // "observable drop notice on the human edge". Behaviorally NEUTRAL on replay (trace/observability
    // only, NOT a stratum-1 Logical Input — the two-strata convention), so the human sees the loss
    // yet it never folds into the World on replay.
    MessageDropped { at: WallClock, edge: EdgeId, reason: DropReason, dropped: u32 },
    // Write-ahead dispatch-intent — the ONE stratum-2 record that is LOAD-BEARING FOR RECOVERY
    // (not neutral). The live driver appends it BEFORE performing an effectful Command; on
    // resume a `cmd` bearing this record with NO matching result Input is an outstanding effect
    // to reconcile. Carries the idempotency `key` (restart-retry dedup token), the input
    // `fingerprint` (binds the eventual result back to this intent), AND the `ctx: ActorCtx`
    // (Theme 1b) — the durable `cmd → ctx` index that lets every result Input inherit its
    // routing/causal facts (`entity`/`origin`/`edge`) from the dispatch that caused it. The
    // replay driver ignores it; only the resume path reads it.
    CommandDispatched { at: Tick, cmd: CmdId, kind: EffectKind,
                        ctx: ActorCtx, key: IdempotencyKey, fingerprint: Fingerprint },
}
// Actor/routing context attached to every entity-scoped dispatch (Theme 1b). It is the durable,
// per-`cmd` record of WHO the command acts for (`entity`), under WHICH causal origin (`origin`),
// on WHICH counterpart relationship (`edge`). Every result Input inherits its `entity`/`origin`/
// `edge` from the matching `CommandDispatched.ctx`, so a derived UI effect is attributable and a
// result routes to the right entity/edge without a positional scan (fixes Coherence "Result entity
// routing lacks a durable source" AND Differentiators "Derived UI effects lose causal context").
struct ActorCtx { entity: EntityId, origin: Origin, edge: EdgeId }
// Which effect a dispatch-intent stands for (selects the resume reconciliation branch).
enum EffectKind { CallModel, RunTool, RequestHumanAction, SendPeer, ScheduleTimer, Compact }
// Why a `MessageDropped` notice fired (Theme 5d).
enum DropReason { InboxOverflow }   // DropOldest at the Inbox cap (Boundedness)
// Dedup token a restart-retry presents so the effect commits effectively-once. Unique per
// effect: App scope × dispatch tick × intra-tick effect ordinal.
struct IdempotencyKey { app_id: AppId, tick: Tick, effect_id: EffectId }
type EffectId = u32;          // core-generated, deterministic: the INTRA-TICK ORDINAL of an
                              // effectful Command within its emitting step — see *Correlate by
                              // identity* (deterministic, core-generated ids)
struct AppId(Opaque);         // App process identity, supervisor-owned (see docs/app/supervisor.md)
struct Fingerprint(Opaque);   // a CANONICAL hash of a request payload (CallModel messages/tools/
                              // params, RunTool tool+args, …). Recorded on the dispatch-intent
                              // AND echoed on the DERIVED result, so replay reuses a recorded
                              // result ONLY when the re-emitted request re-hashes identically; a
                              // mismatch ⇒ stale ⇒ live execution. See *Correlate by identity*.
```

### Commands (World → shell)

```rust
enum Command {
    // Effectful Commands carry `key: IdempotencyKey` so a crash-resume re-dispatch is deduped
    // downstream (effectively-once = at-least-once + idempotency — Intent before commitment;
    // there is no true exactly-once). The cancel/abort duals need NO key: aborting an already-
    // aborted effect is inherently idempotent. `RaiseInteraction` is deduped by its stable
    // `request_id`, so it needs no separate key either.
    CallModel        { cmd: CmdId, entity: EntityId, messages: Vec<Msg>, tools: ToolSet, params: ModelConfig, key: IdempotencyKey },
    CancelInference  { cmd: CmdId },
    RunTool          { cmd: CmdId, entity: EntityId, tool: ToolName, args: Json, key: IdempotencyKey },
    CancelTool       { cmd: CmdId },                          // abort an in-flight RunTool (→ ToolAborted)
    // Boundedness — context-window compaction: summarize the oldest `upto` History Msgs into one
    // summary Msg so the next request's context window shrinks. A model call (carries `key`); → Compacted.
    Compact          { cmd: CmdId, entity: EntityId, upto: u32, key: IdempotencyKey },
    RequestHumanAction { cmd: CmdId, entity: EntityId, ask: HumanAction, notify: Notify, key: IdempotencyKey },
    AbortHumanAction { cmd: CmdId },                          // abort a pending human ask (→ HumanActionAborted)
    RaiseInteraction { request_id: ReqId, entity: EntityId, interaction: AgentInteraction },
    // send/receive duals the Cardinality pass flagged as missing (supplied here by Totality):
    // `SendPeer`'s ONE dual is the sender's local ack `PeerSendOutcome` (Theme 4b); the receiver-side
    // `DriveRequested`/`PeerDelivered` are EXOGENOUS envelope-delivery inputs on the OTHER World, not
    // a dual of this Command. `payload` is `PeerPayload` so a non-drive message is expressible too.
    SendPeer         { cmd: CmdId, to: PeerId, payload: PeerPayload, key: IdempotencyKey },  // → PeerSendOutcome (sender's local ack)
    ScheduleTimer    { cmd: CmdId, id: TimerId, after: Duration, key: IdempotencyKey },      // pairs Timer
}
type Duration = u64;   // shape only
// What a `SendPeer` carries (Theme 4b): a cross-World drive, or a generic peer message.
enum PeerPayload { Drive(DriveCommand), Message(Json) }
```

**Command↔Input duals (Totality — completed).** The Cardinality pass flagged two
effect-result Inputs with no paired originating Command; this pass supplies them, plus the
per-state abort duals so cancellation is total:

- **`SendPeer` ↔ `PeerSendOutcome`** — the SENDER's local delivery ack, and the ONLY dual of
  `SendPeer` (Theme 4b). The receiver-side `DriveRequested`/`PeerDelivered` are NOT duals of this
  Command: they are EXOGENOUS envelope-delivery inputs on the OTHER World, correlated to the sender's
  envelope by `PeerEnvelopeId` (the per-`cmd` fingerprint does not cross the process boundary). A
  generic (non-drive) message rides the same path via `PeerPayload::Message`.
- `Timer` ↔ **`ScheduleTimer`**.
- `ToolAborted` ↔ **`CancelTool`**; `HumanActionAborted` ↔ **`AbortHumanAction`**.

**Named exceptions to the dual rule (deterministic in-World derived inputs).** Two DERIVED inputs
have NO shell-dispatched Command/fingerprint and do not violate the dual rule — they are explicit,
named exceptions:

- `ChildReturned` (Theme 5b) — a deterministic IN-WORLD derived input keyed by `(child,
  tool_use_id)`; it is produced when the child entity reaches its `EndTurn` (regenerated by the
  child's own replay), so it has no originating shell Command and no fingerprint.
- `ToolAborted`/`HumanActionAborted` — `cmd`-keyed abort acks of `CancelTool`/`AbortHumanAction`,
  deliberately non-fingerprinted (an abort has no request payload to hash).

Every other effect-result Input now has an originating Command and every Command a terminating
Input. (Pre-existing symmetric pairs: `CallModel`↔`ModelResponded`/`ModelFailed`,
`RunTool`↔`ToolReturned`, `RequestHumanAction`↔`HumanActionDone`,
`CancelInference`↔`InferenceCancelled`, `RaiseInteraction`↔`InteractionAnswer`.) WAL/
idempotency for these effectful Commands is now supplied by *Intent before commitment* below
(write-ahead `CommandDispatched` + per-Command `key`); correlation of acks by stable id and
`fingerprint` (never arrival order) is supplied by *Correlate by identity* below (lens 6).

---

## Systems (pure reducers `(World, Input) → (World', Vec<Command>)`)

### Tick discipline (fixed intra-tick System order)

Exactly **one Input is processed per tick**, and the Systems run in a **FIXED PHASE ORDER** so the
result is a pure, scheduler-independent function of `(World, Input)` — never the order a runtime
happened to poll Systems in. The canonical phase order each tick is:

1. **Lifecycle / gate updates** — fold gate inputs (`Pause`/`Resume`/`ClearPolicyHalt`/`ClearEntityHalt`,
   `SetAutonomy`) and lifecycle/budget-halt state (GateSystem, BudgetSystem's halt set,
   SupervisionSystem's child-liveness check). Determines what is ALLOWED to run this tick.
2. **Intake** — IntakeSystem admits a queued `UserMessage`/Inbox item into a turn iff both gates
   are `Open` (blocked work stays queued; see GateSystem and Invariant 13).
3. **Settle terminal results** — model / tool / child / human / peer results
   (`ModelResponded`/`ToolReturned`/`ChildReturned`/`HumanActionDone`/peer results) ALWAYS fold
   into `History` and their `ResolvingToolUses` slots, **regardless of either gate** (a gate never
   swallows an in-flight result — GateSystem; Invariant 13).
4. **Budget + compaction checks** — BudgetSystem folds `meta.usage` (spend → maybe halt) and
   checks context pressure; CompactionSystem may defer a continuation behind a `Compact`.
5. **Turn advance / continuation decisions** — TurnSystem / ToolSystem / SubagentSystem /
   HumanActionSystem / PeerDriveSystem decide whether a turn INITIATES or CONTINUES (a new
   `CallModel`/`RunTool`/slot-advance), which is what the gates suppress when Closed.
6. **Emit the tick's Command list** — the final ordered `Vec<Command>` for the tick.

**Ordinals are assigned ONLY AFTER the final Command list is produced (phase 6).** `effect_id`
(the intra-tick ordinal of each effectful Command) and command order are derived from that final,
fixed list — so they are DETERMINISTIC and never depend on which System emitted first or on any
scheduler order. This is what makes `IdempotencyKey = (app_id, tick, effect_id)` and replay
reproducible (*Correlate by identity*; *Intent before commitment*). Every System below runs within
this schedule; the per-System bullets describe each System's role within its phase.

- **IntakeSystem** — Inbox → start a turn when entity `Idle`, `WorldGate Open`, AND
  the entity's `EntityGate Open` (both gates must be Open; the two axes are orthogonal); starting
  a fresh run resets `Components.limits.turns` to 0 (loop guard — *Boundedness and backpressure*).
- **TurnSystem** — `Idle → Thinking` (emit `CallModel`); on `ModelResponded` branch
  EXHAUSTIVELY on `meta.stop_reason` (uses Pass 2's `StopReason`):
  - `EndTurn` (text only) → `Idle`.
  - `ToolUse` → branch the ONE assistant turn into `ResolvingToolUses { slots }`, one
    `ToolSlot` per tool_use block, each tagged with its `ordinal` (the block's position in the
    assistant message) and its `kind`: a plain tool block → `Local` (ToolSystem emits `RunTool`);
    a sub-agent-spawn block → `Child(e)` (SubagentSystem spawns/denies); a human-action block →
    `Human` (HumanActionSystem emits `RequestHumanAction`); a peer-drive block → `Peer`
    (PeerDriveSystem emits `SendPeer`). The kinds MIX freely in one turn; each slot resolves
    independently and the entity advances only when EVERY slot is `Done` (assemble one user Msg
    with all results in `ordinal` order). A turn whose only block is a human action is just one
    `Human` slot.
  - `Refusal` → record the refusal block, surface it on the human edge, → `Idle`.
  - `MaxTokens` → record the truncated blocks, append a continuation nudge and re-issue
    (`→ Thinking`, emit `CallModel`); this counts as a turn against `loop_cap`, so the
    continuation chain is bounded by the loop guard (*Boundedness and backpressure*).
  - `PauseTurn` → re-issue `CallModel` to continue the paused server-tool turn (carry the
    paused blocks forward), staying `Thinking` (also a turn against `loop_cap`).
  On `ModelFailed` → record the error, surface it on the human edge, → `Idle`; any restart/
  retry is bounded by `Caps.restart` (exponential backoff + restart-intensity give-up —
  *Boundedness and backpressure*). CLASSIFY the `ModelError`: a TRANSIENT failure
  (`Timeout`/`Overloaded`/`Http(5xx)`/`Transport`) is eligible for a bounded, accounted AUTO-retry —
  a FRESH `CallModel` (new `cmd`, new `key`) gated by Autonomy AND remaining spend budget, under the
  SAME `Caps.restart` exponential backoff + restart-intensity give-up as the crash-retry; a TERMINAL
  failure (`Http(4xx)`/auth/invalid-request/refusal-as-error) is surfaced on the human edge with NO
  auto-retry.
- **ToolSystem** — owns the `Local` slot kind: emit one `RunTool` per `Local` `ToolSlot`, recording
  the emitted `cmd` in the slot's `Pending { cmd: Some(cmd) }`; on each `ToolReturned`, record its
  `ToolResult` into the `Local` slot whose `Pending` `cmd` matches (slot identity is the `cmd`, not
  arrival order; a tool ERROR rides as `is_error: true` in that same `ToolResult`, so the model sees
  it next turn) and set the slot `Done`. Only once EVERY slot of the `ResolvingToolUses` turn is `Done` (across ALL kinds,
  not just `Local`) append EXACTLY ONE user `Msg` carrying all slot results as `ToolResult` blocks
  **in ascending `ordinal` order**, then continue (`→ Thinking`, emit `CallModel`). Each such
  continuation increments the entity's `EntityLimits.turns`; when `turns` reaches `loop_cap` the loop
  guard WITHHOLDS tools and forces a final wrap-up turn (injecting an `is_error` `ToolResult`, reusing
  Pass 3's recovery rule) so the model concludes instead of looping (*Boundedness and backpressure*).
  On `Cancel` (via CancelSystem) emit one `CancelTool` per `Pending` `Local` slot and enter
  `Cancelling`.
- **SubagentSystem** — owns the `Child` slot kind: for each `Child` `ToolSlot`, if `depth <
  depth_cap` AND the subtree's `EntityLimits.spawned < fanout_cap` spawn a child Entity, bump
  `spawned`, and bind it into the slot (`SlotKind::Child(child_entity)`, `Pending { cmd: None }` —
  a child has NO shell-dispatched command, so the slot is resolved by `(child, tool_use_id)` identity,
  not by `cmd`); if `depth == depth_cap`
  OR `spawned == fanout_cap` DENY the spawn and resolve the slot INLINE with a `ToolResult {
  is_error: true }` ("sub-agent depth cap reached" / "fan-out budget exhausted") — slot `Done`
  immediately (B6 — a denied spawn must not deadlock; the fan-out cap bounds a flat spawn storm
  that `depth_cap` alone does not — *Boundedness and backpressure*). A spawned child's completion
  arrives as a `ChildReturned { parent, child, tool_use_id, result }` Input (correlated by `child`
  + `tool_use_id`): record its `result` into the matching `Child` slot and set it `Done`. When a
  child Entity reaches a TERMINAL non-viable state — crash-reconciliation with no recovery, or
  `EntityGate::Halted` that cannot proceed — the **SupervisionSystem** (or this explicit rule in
  `SubagentSystem`) detects it and resolves that child's `Child` slot with an `is_error`
  `ToolResult` carrying the failure reason (slot `Done`), exactly as a denied spawn does — so a
  dead child can never leave the parent waiting forever (the universal TOTALITY INVARIANT). The
  parent advances by the SAME all-slots-`Done` rule (one user `Msg`, all results in `ordinal`
  order, `→ Thinking`, emit `CallModel`); if every spawn was denied, all `Child` slots are `Done`
  immediately and the turn proceeds without blocking. On `Cancel` (via CancelSystem) cascade
  `Cancel` to each live child, resolve any still-`Pending` `Child` slot with an `is_error` result,
  and settle via `Cancelling`.
- **InteractionSystem** — emit `RaiseInteraction`; on `InteractionAnswer`, integrate.
- **HumanActionSystem** — owns the `Human` slot kind: emit `RequestHumanAction` per `Human`
  `ToolSlot`, recording the emitted `cmd` in the slot's `Pending { cmd: Some(cmd) }`; on
  `HumanActionDone` resolve the `Human` slot whose `Pending` `cmd` matches, by `HumanResult` (slot
  `Done`): `Provided` →
  the answer becomes the slot's `ToolResult`; `Declined`/`Timeout` → an `is_error` `ToolResult`
  into the slot (every `Human` slot backs a tool_use block, so the model always gets a
  `tool_result`). The turn continues by the SAME all-slots-`Done` rule (one user `Msg` in `ordinal`
  order, `→ Thinking`, emit `CallModel`). A turn whose only slot is `Human` settles the same way.
  On `Cancel` (via CancelSystem) emit `AbortHumanAction` per `Pending` `Human` slot and enter
  `Cancelling` (settles on `HumanActionAborted`).
- **SteeringSystem** — `UserMessage` while `Thinking` → enqueue (+ optional Cancel).
- **CancelSystem** — `Cancel` is defined in EVERY active state (Totality): `Thinking →
  Cancelling` (emit `CancelInference`); `ResolvingToolUses → Cancelling` cancels EVERY `Pending`
  slot by its kind (each `Local`/`Human`/`Peer` slot's owed `cmd` is its `Pending { cmd: Some(cmd) }`;
  a `Child` slot, `Pending { cmd: None }`, is identified by `(child, tool_use_id)`) —
  `CancelTool` per `Local`, `AbortHumanAction` per `Human`, `Cancel`-cascade to
  each live `Child`; a `Peer` slot's durable send is NOT recallable (it already sits in the
  at-least-once outbox), so cancellation merely STOPS awaiting that slot and records its owed
  `PeerSendOutcome` in `awaiting`, absorbed (its result discarded) when it arrives — recording all
  owed acks, including a racing `Compacting` turn's owed `Compacted`/compaction `ModelFailed`, in
  `awaiting` (a `Done` slot keeps its result); `Compacting → Cancelling` (emit `CancelInference`); `Idle`/`Cancelling`
  → no-op WITH user feedback (nothing in flight). In `Cancelling`, arbitration is
  keyed on `(cmd, state)`, NOT on a cmd-id mismatch (the `cmd` still MATCHES in `Cancelling`):
  the FIRST terminal input for a given `cmd` — the abort ack (`InferenceCancelled`/`ToolAborted`/
  `HumanActionAborted`) OR a real result that races the abort — clears its cmd from `awaiting`;
  any LATER terminal input for that same `cmd` is idempotently dropped (exactly one terminal
  input per `cmd` — see *Correlate by identity*). When `awaiting` is empty `→ Idle`, keeping any
  `partial`.
- **GateSystem** — owns the App-WIDE `WorldGate`. `Pause{User}`/`Pause{PolicyHalt(trip)}`
  ADD a `PauseHold` (idempotent per source); `Resume` removes every `User` hold;
  `ClearPolicyHalt{trip, authority}` removes the matching `PolicyHalt(trip)` hold (the input
  MUST carry an authority token — its verification is a later enforcement concern, but the
  transition EXISTS here). The gate returns to `Open` ONLY when `holds` is empty, so a
  policy halt survives a user resume until its authorized clearance arrives. **A closed gate
  blocks NEW WORK, never result-settling.** A Paused `WorldGate` or an `EntityGate::Halted` blocks
  turn INITIATION and CONTINUATION ONLY — no new `CallModel`/`RunTool`/`RequestHumanAction`/
  `SendPeer`, no slot-advance to the next `CallModel` — so Intake and the turn-advance phase are
  no-ops while gated and a message to a Paused app QUEUES (sticky), does not resume. It NEVER
  blocks folding a terminal result already in flight: `ModelResponded`/`ToolReturned`/
  `ChildReturned`/`HumanActionDone`/peer results are ALWAYS settled into `History` and their
  `ResolvingToolUses` slots (tick phase 3, *Tick discipline*) even while Paused/Halted — only the
  NEXT continuation is DEFERRED until the gate reopens. (This removes the "paused-while-resolving
  swallows the result / entity stuck" bug: a result that lands during a pause is recorded, and the
  entity simply waits at an all-slots-`Done` boundary for the gate to reopen before continuing.)
  The per-entity `EntityGate` is a separate axis; when an entity HALTS (e.g. budget exhaustion)
  with `Pending` slots, those slots get `is_error` `ToolResult`s so it settles instead of
  deadlocking, but any already-landed result is still folded first. (The halt CONDITION —
  spend-budget exhaustion — and EntityGate RE-open are owned by BudgetSystem below.)
- **BudgetSystem** — owns the per-entity budget brakes (*Boundedness and backpressure*). On every
  `ModelResponded`, fold `meta.usage` into `TokenBudget.used` (SPEND) and refresh `context_used`
  from the call's input+output tokens. Then: if `used ≥ limit` set this entity's
  `EntityGate::Halted { BudgetExhausted }` (NOT the world gate) and synthesize an `is_error`
  `ToolResult` into every outstanding `ToolUse` slot so the entity SETTLES rather than deadlocking;
  else if `context_used` is within headroom of `context_limit` and a continuation is pending, hand
  off to CompactionSystem (emit `Compact`, → `Compacting`) BEFORE the next `CallModel`. Spend
  exhaustion HALTS; context pressure COMPACTS — never confused. On `ClearEntityHalt { entity,
  authority }` BudgetSystem REOPENS that entity's `EntityGate::Halted → Open` (the per-entity analog
  of `ClearPolicyHalt`; the input MUST carry an authority token — its verification is a later
  enforcement concern, but the transition EXISTS here) — so a budget-halted entity has a NAMED
  clearing input and is never a permanent sink (Totality).
- **CompactionSystem** — emit `Compact { upto }` to summarize the oldest `upto` History Msgs and
  enter `Compacting`; on `Compacted`, splice the `summary` Msg over those `replaced` Msgs (History
  shrinks, `context_used` drops) and emit the deferred continuation `CallModel` (→ `Thinking`); on
  a compaction `ModelFailed`, proceed UN-compacted as a best-effort continuation (→ `Thinking`) so
  boundedness never introduces a new sink.
- **AutonomySystem** — before a tiered action, emit `RaiseInteraction(Approval)` per
  `Autonomy` policy and HOLD the gated `RunTool`. On `InteractionAnswer`: approve → emit the
  held `RunTool`; reject → resolve that tool's `ToolSlot` with a `ToolResult { is_error: true }`
  ("action denied by user"), slot `Done` (B6 — a rejected Approval must not deadlock), exactly as
  if the tool had returned an error.
- **SurfaceSystem** — maintains the World's view of the client surfaces from TWO inputs of
  DIFFERENT status. A **human-origin** `SurfaceMutated` Input (the only logged `SurfaceMutated`,
  EXOGENOUS) is applied directly — a human grabbing the wheel, keeping mode-as-projection in sync
  (B8). An **agent** UI write is NOT a separate input: SurfaceSystem updates the surface view by
  folding the `set_value` `RunTool`'s `ToolReturned` (a PROJECTION of that single commit record,
  folded exactly once keyed by `cmd`), and the mode fold folds the `Event` WRAPPING that
  `ToolReturned` (`origin = Agent`, `edge` from the dispatch's `ctx`) — never the `RunTool`
  Command, which is never folded directly (Theme 1d) — so there is exactly ONE record of an agent
  UI write (its `ToolReturned`), never a second `SurfaceMutated` log entry (*Single source of
  truth*, lens 8). It applies a `SurfaceObserved` Input to refresh the perceived UI state
  (`surface_version`/`ax_digest`/focus/selection/viewport/window/cursor). CONFLICT POLICY (Theme
  2c): a batch of `surface_ops` applies in `Vec` ORDER; the default is `LastWriterWins` (a later op
  on the same `surface`/`element` supersedes an earlier one); but an op carrying a
  `base_version: Some(v)` that no longer matches the current `SurfaceVersion` is REJECTED with an
  `is_error` result (optimistic concurrency), and each applied change BUMPS that surface/element's
  `SurfaceVersion`. Activity is unchanged by a surface update.
- **PeerDriveSystem** — owns the `Peer` slot kind. Outbound: a `drive_peer` `ToolSlot` emits
  `SendPeer`, recording the emitted `cmd` in the slot's `Pending { cmd: Some(cmd) }`; the sender
  records `PeerSendOutcome { Delivered | Queued | Rejected }` so its own replay is stable, and that
  outcome resolves the `Peer` slot whose `Pending` `cmd` matches (slot `Done`): `Delivered`/`Queued`
  → a success `ToolResult`; `Rejected` → an `is_error` `ToolResult`. (Delivery is durable and
  effectively-once — see *Durable peer delivery*.) Inbound: a `DriveRequested`/`PeerDelivered`
  (origin Peer) BINDS to `edge_for(Counterpart::Peer(from))` (Theme 4a — logging an `EdgeBound` the
  first time that edge is created), so the receiver's edge is in the log, not chosen out-of-band; a
  `DriveRequested` is then authorized, its `surface_ops` applied to this client's surface view as a
  PROJECTION of the `DriveRequested` Input itself (no separate `SurfaceMutated` record — the
  `DriveRequested` log entry IS the record), and any `prompt` enqueued to the Inbox (B10 — the
  cross-World contract is acked and recorded in the target's log). The receiver-side
  `DriveRequested`/`PeerDelivered` are EXOGENOUS envelope-delivery inputs correlated by
  `PeerEnvelopeId`, NOT duals of the sender's `SendPeer` (Theme 4b).

---

## Determinism discipline (no hidden inputs)

Every System is a pure reducer `(World, Input) → (World', Vec<Command>)`: its ONLY inputs
are the `World` it is handed and the one `Input` it is reducing. Anything else a turn
depends on is a **hidden input** that breaks determinism and therefore replay; each such
value must cross a **recorded boundary** (logged as an Input and/or held in a Resource fed
only from the log) before a System may read it.

- **Two drivers, not a flag (B3).** The imperative shell has a **live driver** and a
  **replay driver** — separate drivers, never a boolean inside a System. Both run the
  identical Systems, which emit `CallModel`/`RunTool`/… Commands the same way in both
  cases. The **live driver dispatches** emitted Commands (performing the real effect, whose
  result re-enters as a recorded Input); the **replay driver discards** emitted Commands and
  instead replays the already-logged Result Inputs in their place. *Invariant:* a System
  cannot tell which driver it runs under, and replay re-calls nothing because the replay
  driver suppresses every Command — not because any System behaves differently. The stand-in
  is CONTENT-ADDRESSED, not positional: the replay driver reuses a recorded result only when
  the freshly-emitted request's `fingerprint` matches the recorded one; a mismatch (after an
  edit/fork) makes that branch go LIVE. The run holds a SINGLE mode that flips EXACTLY ONCE: it
  starts in replay and, on the first fingerprint mismatch, transitions one-way to live at that
  point — never back to replay, never both at once — see *Correlate by identity*.
- **Wall-time only from `WallClock`.** Systems read observed wall-time exclusively from
  `Resources.wall` (primed and updated from the `Event.wall` stamp in the log), never the
  host clock. The agent's system-prompt date is a wall read and routes through `WallClock`.
- **Randomness only through `Rng`.** The seed is recorded once (`SessionStarted { seed }`,
  tick-0); a fresh replay World reseeds identically and draws reproduce. No ambient RNG.
- **Routing/capability/reasoning recorded per call.** The effective model id, capability
  metadata, and reasoning round-trip policy a call used are recorded in `ModelResponded.meta`
  — never looked up from a live table at replay time — so replay reproduces the same branch.
- **Deterministic iteration.** Every `Map<…>` in the World schema is a deterministically
  ordered structure (`BTreeMap`, or a generational dense index), with a defined iteration
  order. Systems MUST NOT iterate (fold, collect, or interleave RNG draws over) an exposed
  `HashMap`, whose per-process-random order is a hidden input that diverges `World'` on
  replay.
- **Avoid floats in transitions.** Prefer integers/fixed-point in World state; if a float
  is unavoidable, pin deterministic float behavior so cross-architecture replay matches.
- **Purity is enforced mechanically, not by convention.** The "System is a pure reducer of
  `(World, Input)`" invariant is checked, not trusted: a System receives `&World`/`&Input` with
  no `&mut` ambient access, and a `cargo xtask lint`-style check BANS `SystemTime::now()`,
  `rand::`, `std::env`, and exposed-`HashMap` iteration inside any System — so a hidden-input
  regression fails the BUILD, not merely review.

All other nondeterminism (env, config, capability tables, peer state) is forbidden inside a
System and must enter through a recorded Input the same way. *Noted for later passes (not
this lens):* WAL/idempotency for in-flight effects (lens 5 — now applied; see *Intent before
commitment*), id-correlation of result Inputs (lens 6), boundedness caps (lens 7), and the
single-source dedup of per-call capability records vs. any table (lens 8 — now applied; see
*Single source of truth*).

---

## Intent before commitment (write-ahead logging)

An effect that was **dispatched but not yet resolved** is the dangerous window. The log records
an effect only once its RESULT lands — the result Input (`ModelResponded`, `ToolReturned`, …) IS
the commit record — so a crash between dispatch and result leaves NO trace of the in-flight
effect: on the next boot it is silently LOST, or blindly re-run and DUPLICATED. The fix is the
database **write-ahead** rule: **record intent before acting, make the effect idempotent, and
reconcile on recovery.** Exactly-once delivery is impossible; what we engineer is
**effectively-once = at-least-once + idempotency** — never a true exactly-once.

- **Write-ahead dispatch-intent (stratum 2, load-bearing).** Before the **live driver** performs
  an effectful Command it appends a `CommandDispatched { cmd, kind, key, fingerprint }` record.
  It is **Stratum 2** because it is OPERATIONAL — it carries no Logical Input and replay never
  folds it into the World — but it is the ONE stratum-2 record that is **load-bearing for
  recovery**, unlike the behaviorally-neutral `WorkerCrashed`/`WorkerRespawned`/`Tombstoned`/
  `Restored`. On resume, a `cmd` bearing a `CommandDispatched` with **no matching result Input**
  is an **outstanding effect** to reconcile. (The replay driver ignores the record; only the
  resume/recovery path reads it.)
- **Log-before-apply invariant.** No World mutation is applied before its causing Input is
  **durably appended** to the stratum-1 log. The World therefore stays a pure function of the
  durable log: a crash can never lose an applied-but-unlogged result, and replay reconstructs
  exactly the committed World. (Symmetric with write-ahead: intent is logged before the effect;
  the result is logged before it mutates the World.)
- **Idempotency keys on non-idempotent Commands.** `CallModel`, `RunTool`, `RequestHumanAction`,
  `SendPeer`, `ScheduleTimer`, and `Compact` (Theme 5c — `Compact` is itself a model call) carry
  `key: IdempotencyKey = (app_id, tick, effect_id)`. A
  restart-retry re-presents the SAME key so the downstream dedupes the second attempt:
  **provider-side** for `CallModel` and `Compact` (idempotency header), **per-tool contract** for
  `RunTool` (and the client `set_value` for UI writes), **broker-side** for `SendPeer`. The cancel/abort
  duals (`CancelInference`/`CancelTool`/`AbortHumanAction`) carry no key — aborting an already-
  aborted effect is inherently idempotent; `RaiseInteraction` is deduped by its stable
  `request_id`.
- **Tool effect classification.** `ToolSet` carries a per-tool `ToolEffect`
  (`Observational` vs `Effectful { idempotent }`) because reconciliation policy DIFFERS: an
  Observational tool is safe to re-run blindly; an Effectful tool is re-run ONLY if its contract
  is `idempotent` (re-present the key), otherwise it is **surfaced-as-failed** (`is_error`
  ToolResult) rather than silently retried.
- **Reconciliation = the replay→resume edge.** Recovery is two ASYMMETRIC phases:
  - **replay** rebuilds the World by consuming logged stratum-1 Inputs and **suppressing** every
    emitted Command (the replay driver — *Determinism discipline*); then
  - **resume** (live recovery) inspects each entity's **tail Activity** — a waiting state whose
    outstanding `cmd` has a dispatch-intent but no result Input — and **re-dispatches or settles**
    per the per-effect policy below.

  | Tail (outstanding `cmd` / slot) | Reconciliation |
  |---|---|
  | `Thinking` (CallModel) | Synthesize + LOG a stratum-1 `InferenceCancelled { reason: Crash, partial: None }`; the entity settles to `Idle` and Autonomy/budget decides a FRESH, accounted retry (new `cmd`, new `key`). The crash is a real logical Input, not a silent rewind. |
  | `ResolvingToolUses` — `Local` slot (RunTool) | Observational → re-dispatch (same `key`). Effectful + `idempotent` → re-dispatch (same `key`; downstream dedupes). Effectful + non-idempotent → resolve the slot with an `is_error` `ToolResult` ("effect interrupted by crash; not safely retryable"). |
  | `ResolvingToolUses` — `Human` slot (RequestHumanAction) | Re-dispatch (same `key`) — the notify is idempotent per key, so the human is not double-prompted; a still-pending ask simply re-attaches to its slot. |
  | `ResolvingToolUses` — `Child` slot | Each child reconciles via its OWN tail; an unrecoverable child resolves its `Child` slot with an `is_error` `ToolResult` (SupervisionSystem, Totality). |
  | `ResolvingToolUses` — `Peer` slot (SendPeer) | Re-dispatch (same `key`); the broker dedupes by envelope id (*Durable peer delivery*) and the `PeerSendOutcome` resolves the slot as before. |
  | `ScheduleTimer` outstanding | Re-dispatch (same `key`); the timer dedupes and the `Timer` result lands as before. |
  | `Compacting` (Compact) | Re-dispatch (same `key`) — `Compact` is a model call, provider-deduped by key (Theme 5c); the deferred continuation stays deferred until it settles: `Compacted` splices the summary then emits the continuation, or a compaction `ModelFailed` proceeds un-compacted (best-effort) — consistent with Invariant 5 / CompactionSystem. |

- **The crash's behavioral effect is a stratum-1 Input — amends "behaviorally neutral".** The
  lifecycle `WorkerCrashed`/`WorkerRespawned` records STAY behaviorally neutral on replay (trace
  only). But the crash's BEHAVIORAL consequence — a lost in-flight effect — is NOT neutral; it is
  realized as a real **stratum-1** Input (the synthesized `InferenceCancelled { reason: Crash }`
  or `is_error` `ToolResult` above), so budget accounting and Autonomy see it and replay
  reproduces it deterministically. The neutral lifecycle event stays; the behavioral effect moves
  into the logical log.
- **UI-write WAL.** Agent UI writes ride an Effectful `RunTool` (the UI tools / the underlying
  `set_value`), so they inherit the write-ahead `CommandDispatched` + idempotency `key`; the
  shell presents that key to the client so a resume re-dispatch does NOT double-apply to the
  screen and world↔screen cannot desync. The agent UI write IS the `set_value` `RunTool`; its
  `ToolReturned` — wrapped in an `Event` with `origin = Agent` and `edge` from the dispatch's
  `ctx` — is the SOLE commit record and the thing the mode fold folds over (Theme 1c/1d). **NO
  agent-origin `SurfaceMutated` is ever logged** — the agent surface change is a pure projection
  of that `ToolReturned`.

A resume-synthesized result (the Crash `InferenceCancelled`, a crash `is_error` `ToolResult`)
fills `cmd`/`entity`/`fingerprint` from the outstanding `CommandDispatched` record, so it
correlates by IDENTITY exactly like a real result — see *Correlate by identity*.

*Resolved by Single-source (lens 8):* the dispatch-intent log reconciles against NO side store —
`resume.json` is eliminated and all restore state rebuilds from the log (see *Single source of
truth*). (Retry/backoff CAPS on the fresh post-crash
retry — formerly deferred here — are now supplied by *Boundedness and backpressure* below
(`Caps.restart`: exponential backoff + restart-intensity give-up). The `effect_id` GENERATION
scheme and the `fingerprint`→result binding are supplied by *Correlate by identity* below.)

### Durable peer delivery (outbox/inbox protocol)

`SendPeer` crosses the process boundary, so its `key`/`fingerprint` (per-`cmd`, in-process) do NOT
reach the receiver — the cross-process correlation handle is the sender-assigned, retry-stable
`PeerEnvelopeId` (*Correlate by identity*). The delivery protocol makes peer drives durable and
**effectively-once = at-least-once + idempotency-by-envelope-id**, WITHOUT any atomic cross-log
transaction (there is none — the two Apps own independent logs):

1. **Broker fsyncs the envelope.** On `SendPeer` the broker durably records the envelope (id +
   payload + auth) with an `fsync` before it attempts delivery, so a crash cannot lose an accepted
   drive; an undelivered envelope to an offline peer sits in the durable outbox (`PeerSendOutcome::
   Queued`) and is retried on the peer's restore.
2. **Receiver appends idempotently, keyed by envelope id.** The receiver appends the inbound
   `DriveRequested`/`PeerDelivered` to ITS stratum-1 log **idempotently keyed by `envelope`**: a
   redelivery whose `PeerEnvelopeId` is already present is deduped (logged-as-trace, not re-applied),
   so an at-least-once retry lands AT MOST ONCE in the receiver's log.
3. **Sender records `Delivered` only after a durable receiver ack.** `PeerSendOutcome::Delivered` is
   recorded on the sender ONLY after the receiver durably appended (and acked) the envelope; until
   then the outcome is `Queued` (retry pending) or `Rejected` (inbox cap / auth fail). The sender's
   own `Delivered` is thus never optimistic.

**Crash model.** A crash between the receiver's durable append and the sender's ack leaves the
sender to RETRY the same envelope; the receiver dedupes it by `envelope` id and re-acks — so the
worst case is a retried, idempotently-deduped delivery, **never two disagreeing logs**. There is no
two-phase commit across the two Apps: each log is independently durable, and the envelope id is the
sole reconciliation key (the broker outbox is itself reconstructable from the sender's
`PeerSendOutcome{Queued}` + the receiver's `DriveRequested`/`PeerDelivered` — *Single source of
truth*).

---

## Correlate by identity, not position (correlation identifier)

Every async request↔result pair is matched by a **stable, deterministically-generated,
content-meaningful key** — never by log position or arrival order. Two corollaries: replay
reuses a recorded result only when it still answers the SAME request, and a late or duplicate
ack is routed and dropped by identity, not silently mis-applied to whatever sits at that
position.

### Content-addressed replay — the fingerprint (B4)

Positional replay is the headline replay-correctness trap. After a rewind+edit, a recorded
`ModelResponded` answered the OLD prompt; replaying it by TICK POSITION would feed that STALE
answer to the edited prefix. The fix is to address every DERIVED result by the IDENTITY of the
request that produced it, not by where it sits in the log.

- **`Fingerprint` = a canonical hash of the exact request payload.** For `CallModel` it hashes
  the assembled `messages`/`tools`/`params` (the same bytes `MessageBuilder` would serialize);
  for `RunTool`, the `tool`+`args` (plus the perceived `surface_version`/`ax_digest` for a UI tool —
  Theme 2b); for `RequestHumanAction`/`ScheduleTimer`/`SendPeer`, the `ask`/`(id,after)`/`payload`.
  *Canonical* = a fixed field order and encoding with no map-iteration
  or float nondeterminism (*Determinism discipline*), so identical requests hash identically
  across processes and runs.
- **Recorded on the result, checked on re-emit.** Every DERIVED result Input carries the
  `fingerprint` of the request that produced it (and the dispatch-intent `CommandDispatched`
  records the same value). On re-run, when a System emits the Command for `cmd`, the **replay
  driver** recomputes the freshly-emitted request's fingerprint and compares it to the recorded
  result's:
  - **match → reuse** the recorded result (suppress the Command, feed the cached Input);
  - **mismatch → the recorded result is STALE** for this branch → discard it and let the Command
    fall through to **LIVE execution**; every later effect on that branch is live too.
- **One rule for fork AND edit/rewind.** A fork that changes nothing re-fingerprints identically
  and reuses the entire tail; a fork that edits a prompt re-fingerprints the first affected
  `CallModel` differently and goes live from exactly that point onward. Edit/rewind is just a
  fork whose divergence point is the edit — the fingerprint check localizes the live boundary
  without any position arithmetic, and the same machinery serves both.
- **One-way `replay → live` handoff (single mode, flips exactly once).** A run is in EXACTLY ONE
  mode at any instant. It BEGINS in REPLAY mode, where the replay driver matches each
  freshly-emitted Command's `fingerprint` to the recorded result; on the FIRST mismatch the run
  transitions to LIVE mode AT THAT POINT — the mismatched Command is dispatched live, and every
  SUBSEQUENT Command on that branch is dispatched live too. The transition is one-way and fires
  exactly once: there is NO return to replay, NO concurrent replay+live, and NO queue ambiguity.
  This first-mismatch boundary IS the fork/edit boundary.
- **Edit scope — exogenous inputs only.** An edit/rewind targets EXOGENOUS inputs ONLY
  (`UserMessage`, `Pause`/`Resume`/`Cancel`, `InteractionAnswer`, `SetAutonomy`, `PeerDelivered`,
  and human-origin `SurfaceMutated`). DERIVED inputs (model/tool results) are NEVER edited in
  place — to change one you REWIND and let the call re-run live; the re-emitted request then
  re-fingerprints differently and the branch goes live by the rule above.

### Fingerprint vs external state — per-command replay policy

A `Fingerprint` covers the REQUEST payload only. That is total for a `CallModel` (the provider is
stateless — same request, same recorded answer), but a tool or peer result can depend on EXTERNAL
state NOT in the request: a file's bytes, the surface's current value, the peer log's head, an auth
epoch, a remote resource's ETag. A request that re-hashes identically can therefore have a
DIFFERENT live result than the recorded one — so a fingerprint match alone does NOT prove the
recorded result is still valid. Each tool/command carries a **replay policy**, tied to its existing
`ToolEffect` classification:

- **Pure / Deterministic** (typically `Observational` over inert input). The result is a function
  of the request alone → on a fingerprint MATCH, **reuse the recorded result** (the default rule).
- **External-state.** The result depends on state outside the request → a bare fingerprint match is
  insufficient. Such a command takes ONE of two stances:
  - **(a) state-version token in the fingerprint** — fold the relevant external version into the
    hashed request: a file digest, the surface version, the peer-log head, the auth epoch, an ETag,
    or an as-of timestamp. The fingerprint then DIVERGES exactly when the external state changed, so
    the existing match→reuse / mismatch→live rule stays correct.
  - **(b) force-live-after-divergence** — mark the command so that, once a run has gone LIVE (past
    the first-mismatch boundary), it **re-runs live rather than reusing a possibly-stale recorded
    result**, even on a fingerprint match. (Before any divergence, on an UNCHANGED replay, the
    recorded result still stands in — the external world is assumed unchanged for a faithful
    re-run.)

`Observational` tools are the safe-to-reuse / safe-to-re-run-live class; `Effectful` tools already
re-run only under an idempotency contract (*Intent before commitment*), and an External-state
`Effectful` tool combines that with (a) or (b) above. The choice is per-tool metadata, recorded so
replay is deterministic.

**UI / `set_value` tools are External-state with stance (a) (Theme 2b).** Their `Fingerprint` MUST
fold in the perceived `surface_version`/`ax_digest` that the manipulation depended on (recorded by
the `SurfaceObserved` Input), so a re-emitted UI request DIVERGES exactly when the macOS UI state it
read changed and the existing match→reuse / mismatch→live rule stays correct. This closes the
"macOS UI state is a hidden input" replay hole: the perceived surface state now crosses a recorded
boundary instead of being read ambiently at replay time.

### EXOGENOUS vs DERIVED (the basis of the rule)

Stratum 1 is NOT homogeneous; the `LogicalInput` schema now marks two correlation classes:

- **EXOGENOUS inputs are free variables** — `SessionStarted` (the tick-0 header — Theme 5a),
  `UserMessage`, `Pause`/`Resume`/`Cancel`/`ClearPolicyHalt`/`ClearEntityHalt`, `InteractionAnswer`,
  `SetAutonomy`, `PeerDelivered`, `DriveRequested`, `EdgeBound` (receiver edge binding — Theme 4a),
  `SurfaceObserved` (UI perception — Theme 2b), and **human-origin** `SurfaceMutated` (the ONLY
  logged `SurfaceMutated`). They have no originating Command, so an edit/rewind carries them across
  VERBATIM; they are never fingerprinted.
- **DERIVED inputs are an effect-result cache** — `ModelResponded`/`ModelFailed`, `ToolReturned`,
  `ChildReturned`, `InferenceCancelled`, `HumanActionDone`, `Timer`, `PeerSendOutcome`, `Compacted`,
  and the abort acks `ToolAborted`/`HumanActionAborted` (Theme 5a). Each is valid ONLY while its
  request still re-resolves the same way: the fingerprinted results are reused on `fingerprint` match
  (subject to the per-command replay policy above for External-state tools), else stale → live. THREE
  are DERIVED but correlated by IDENTITY rather than a request fingerprint, and are deliberately
  NON-fingerprinted: `ChildReturned` (`child` + `tool_use_id`, regenerated deterministically by the
  child's own replay above), and the abort acks `ToolAborted`/`HumanActionAborted` (keyed by `cmd` —
  an abort has no request payload to hash). The **agent-origin** surface change is NOT in this list:
  it is a PROJECTION of the `set_value` `RunTool`'s `ToolReturned`, not an independent input (*Single
  source of truth*, lens 8), so the only `SurfaceMutated` Input is the EXOGENOUS human-origin one.

### Deterministic, core-generated ids

`cmd`, `request_id`, `effect_id`, and `edge_id` are PURE functions of World state — never
shell-assigned. A shell-assigned id is unrecorded nondeterminism: two runs would mint different
ids and no recorded result could be correlated back on replay. They come from the monotonic
allocator Resource `Resources.ids: IdAlloc`:

- `cmd: CmdId`, `request_id: ReqId`, and `edge_id: EdgeId` each draw the next value from their
  counter and bump it INSIDE the pure reduce step, so replay mints byte-identical ids.
- `effect_id: EffectId` is the **intra-tick ordinal** of an effectful Command within the step
  that emits it (the n-th effectful Command this tick), so `IdempotencyKey = (app_id, tick,
  effect_id)` (Intent before commitment) is unique without a persistent counter — grounding that
  pass's deferral.
- A `TimerId` is likewise core-minted when a `ScheduleTimer` is emitted.

### Result Inputs route by `entity`; exactly one terminal input per `cmd` (B11)

- **`entity`/`origin`/`edge` on every result Input, inherited from the dispatch (Theme 1b).**
  Each entity-scoped Command (`CallModel`, `RunTool`, `RequestHumanAction`, `SendPeer`,
  `ScheduleTimer`, `Compact`, and the cancels) is dispatched with an `ActorCtx { entity, origin,
  edge }`, recorded on its `CommandDispatched.ctx` — the durable, per-`cmd` `cmd → ctx` index.
  Every result Input — `ModelResponded`/`ModelFailed`/`ToolReturned`/`HumanActionDone`/
  `PeerSendOutcome`/`Compacted`/`InferenceCancelled`/`ToolAborted`/`HumanActionAborted`/`Timer` —
  INHERITS its `entity`/`origin`/`edge` from the matching `CommandDispatched.ctx`, so routing is
  unambiguous WITHOUT scanning for the matching Activity by position AND a derived UI effect stays
  attributable to the turn that caused it. The `entity` field carried on a result is exactly this
  inherited value; a late result for an already-cancelled or already-finalized `cmd` is matched to
  its `ctx` and idempotently dropped, never misrouted. The `ctx` index — not a second source of
  truth — is itself a pure fold of the `CommandDispatched` records.
- **Cancel arbitration re-keyed on `(cmd, state)`.** "Dropped by cmd-id mismatch" MISFIRES,
  because in `Cancelling{awaiting}` (and in any awaiting state) the `cmd` still MATCHES — a
  mismatch test would discard the very ack it waits for. Instead: in a cancelling/awaiting state
  the **FIRST** terminal input for a given `cmd` — whichever of `ModelResponded` (the call
  completed first) or `InferenceCancelled` (the abort won) arrives first — finalizes that cmd;
  any LATER terminal input for the **same `cmd`** is **idempotently dropped** (logged-as-trace
  only).
- **Shell contract: exactly one terminal input per `cmd`.** The shell ALWAYS delivers a terminal
  input for an outstanding `cmd` — it **synthesizes `InferenceCancelled` even if the underlying
  call already completed** — so no `cmd` is left waiting and the race is decided by identity and
  arrival-of-first, never by position. The World-side `(cmd, state)` drop is the defensive net if
  a duplicate (e.g. a resume-synthesized Crash ack racing a real result) still slips through.

### Correlation-pair audit

Every async pair uses a stable key, not arrival order:

| Pair | Stable key | Status |
|---|---|---|
| `ToolUse` ↔ `ToolResult` | `tool_use_id == ToolUse.id == ToolResult.tool_use_id` | keyed (Anthropic contract) |
| `CallModel`/`RunTool`/… ↔ result Input | `cmd: CmdId` + `fingerprint` | keyed + content-addressed |
| `RaiseInteraction` ↔ `InteractionAnswer` | `request_id: ReqId` | keyed |
| parent `Child` slot ↔ `ChildReturned` | `(child: EntityId, tool_use_id: ToolUseId)` carried on `ChildReturned` | keyed (identity, not fingerprint) |
| `SendPeer` ↔ `PeerSendOutcome` | `cmd: CmdId` | keyed |
| `Compact` ↔ `Compacted` | `cmd: CmdId` + `fingerprint` | keyed + content-addressed |
| sender envelope ↔ `PeerDelivered`/`DriveRequested` | `envelope: PeerEnvelopeId` | keyed (sender-assigned; see note) |
| in-flight effect ↔ dispatch-intent | `cmd` + `key: IdempotencyKey` | keyed (Pass 5) |

None rely on log position or arrival order. The cross-app envelope key (`PeerEnvelopeId`) is
**sender-assigned** and stable across the sender's retries, so an at-least-once redelivery is
de-duped by the receiver on IDENTITY rather than counted twice; the per-`cmd` `fingerprint` rule
does not cross the process boundary, so the envelope id is the correlation handle there.

---

## Boundedness and backpressure

Every loop carries a step bound; every queue a cap with a defined overflow behavior; every
monotonically-growing structure a compaction or a snapshot; every retry an exponential backoff
with a give-up bound. No structure in the World — or in the supervisor that hosts it — may grow
without a brake (Reactive Streams backpressure; control theory: bound the integrator or it winds
up). The brakes are World state — config singletons (`Resources.loop_cap`/`fanout_cap`/`caps`) and
per-entity counters (`Components.limits`, `TokenBudget`) — so they replay deterministically like
everything else.

### Two budgets: spend (halt) vs context window (compact)

These are DISTINCT quantities with distinct responses, and conflating them is the classic bug:

- **Spend budget** — `TokenBudget.used` / `.limit`. CUMULATIVE token cost over the whole
  trajectory; monotonically growing. **BudgetSystem** folds each `ModelResponded.meta.usage` into
  `used`; when `used ≥ limit` the entity is HALTED — `EntityGate::Halted { BudgetExhausted }` (the
  PER-ENTITY gate, never the world gate, so one sub-agent exhausting its budget does not freeze the
  App). Any outstanding `ToolUse` slots get a synthesized `is_error` `ToolResult` so the entity
  SETTLES instead of deadlocking (the Totality recovery rule).
- **Context window** — `TokenBudget.context_used` / `.context_limit`. The SINGLE-REQUEST input size
  the next `CallModel` would carry; it does NOT grow monotonically because compaction shrinks it.
  When `context_used` approaches `context_limit` the entity COMPACTS rather than halts.

A halt is terminal until the budget is raised/cleared (an enforcement concern); a compaction is a
routine in-band operation that lets the run continue within the same spend budget.

### Context-window compaction

**CompactionSystem** + the `Compact` Command + the `Compacted` Input bound the per-request context:

- When BudgetSystem detects context pressure (`context_used` within headroom of `context_limit`)
  as a continuation `CallModel` is about to be emitted, it emits `Compact { upto }` instead and the
  entity enters `Compacting` (the continuation is DEFERRED).
- `Compact` summarizes the oldest `upto` History Msgs via a model call (so it carries an idempotency
  `key` and is fingerprinted like any request). Its result, `Compacted { summary, replaced }`,
  splices one `summary` Msg over the oldest `replaced` Msgs: History shrinks and `context_used` drops
  below the limit.
- The entity then emits the deferred continuation `CallModel` (→ `Thinking`). If compaction itself
  fails (`ModelFailed`), the entity proceeds UN-compacted as a best-effort continuation rather than
  stalling — boundedness must not introduce a new sink.

Recent, near-context History stays verbatim; only OLDER turns are summarized, so tool-call fidelity
for the active turn is preserved.

### Loop guard — bound the tool/turn loop

A model that keeps calling tools forever is an unbounded loop. `Components.limits.turns` counts
model turns since the entity's last EXOGENOUS input; IntakeSystem resets it to 0 when a fresh
`UserMessage`/Inbox item starts a run from `Idle`, and it increments on every continuation
`CallModel` (tool batch done, sub-agent batch done, `MaxTokens` continuation, `PauseTurn`
continuation). When `turns` reaches `Resources.loop_cap` the guard trips: the next continuation
WITHHOLDS tools and injects an `is_error` `ToolResult` ("step budget exhausted; finalize now") —
reusing Pass 3's recovery rule — forcing a final text turn that settles the entity to `Idle`. Any
tool block the model still emits past the cap is denied with `is_error`, so the run cannot re-arm
the loop.

### Fan-out budget — bound sub-agent spawning

`depth_cap` bounds NESTING along a path; it does NOT bound the TOTAL number of children a subtree
spawns (a flat fan-out of thousands is within depth 1). `Components.limits.spawned` counts
sub-agents spawned in a subtree against `Resources.fanout_cap`; **SubagentSystem** denies a spawn
once either `depth == depth_cap` OR `spawned == fanout_cap`, synthesizing an `is_error` `ToolResult`
into the spawning `ToolUse` (the same B6 no-deadlock rule). Depth and fan-out are independent bounds.

### Restore snapshots — bound replay, reconciliation, and memory

Replaying the WHOLE logical log on every restore is O(history), so restore cost grows with session
age and a long-lived app's repeated restores degrade toward O(n²) over its lifetime. Because the
World is pure and serializable, the runtime periodically writes a tick-indexed **snapshot** of the
World every `Caps.snapshot_interval` ticks. Restore then loads the **nearest snapshot ≤ target
tick** and replays only the **tail** of the log after it:

- **Replay cost** is bounded by the snapshot interval, not the session length.
- **Crash reconciliation** (the *resume* phase) only inspects the replayed tail's outstanding
  dispatch-intents, so it is bounded to the recent tail too.
- **Memory** is bounded: old log segments fully covered by a later snapshot can be cold-stored; the
  live working set is snapshot + tail.

A snapshot is a DERIVED CACHE of the log — the **logical log stays the single source of truth**, and
a snapshot is only a fast-forward fully reproducible by replay. (Snapshot↔log consistency and the
pruning rule — keep the latest K, regenerate the rest, the log wins on conflict — are specified in
*Single source of truth*, lens 8.)

### Process budget — admission and eviction

Live App processes are themselves a bounded resource (each holds an in-memory World + worker). The
supervisor (see `docs/app/supervisor.md`) enforces a **soft cap** of `N` concurrent `Resident`
processes:

- **Admission.** A message for a Tombstoned app may transiently admit it as the `N+1`-th process,
  then the supervisor converges back to `N` by evicting an idle process; OR, under pressure, the
  restore is QUEUED until a slot frees. (Pick one per deployment; both are bounded.)
- **Eviction is idle-only LRU.** Only a `Resident` app with NO in-flight turn (its root and all
  sub-agents `Idle`) may be Tombstoned to reclaim a slot; it restores transparently later. An app
  with an in-flight turn (`Thinking` / `ResolvingToolUses` / `Compacting` / `Cancelling`) MUST NOT
  be evicted — forcing it out would present as a crash and trigger the *resume* reconciliation path
  needlessly.
- **Starvation backstop.** If every slot is held by a BUSY app (including one parked on a pending
  `Human` slot in `ResolvingToolUses`, awaiting a human answer), nothing is idle to evict, so a
  pending restore does not spin: it waits on a bounded queue and, past a deadline, escalates to the
  supervisor (surface "at capacity" to the human / shed the lowest-priority app under policy) rather
  than livelocking. An app waiting on a `Human` slot in particular can hold a slot indefinitely, so
  the backstop is what keeps a few stuck apps from starving all others.

### Bounded queues — `Inbox`, `RaisedInteractions`, peer inbox

Each queue has an explicit cap (`Resources.caps`) and a DEFINED behavior at the cap:

- **`Inbox`** (`Caps.inbox`, **DropOldest**). A long-`Paused` app keeps receiving `UserMessage`s
  that sticky-queue; without a cap the Inbox would grow unbounded while paused. At the cap the
  OLDEST queued Event is dropped (the newest intent is the most relevant), bounding the backlog. The
  eviction is NOT silent: dropping the oldest queued Event emits an observable drop notice on the
  human edge — a STRATUM-2 `LifecycleEvent::MessageDropped { at, edge, reason: InboxOverflow,
  dropped }` (Theme 5d; trace/observability only), NOT a stratum-1 Logical Input, so the human sees
  the loss yet it never folds into the World on replay (the two-strata convention).
- **`RaisedInteractions`** (`Components.raised`, `Caps.raised`, **RejectNewest**). Pending approvals
  awaiting a human answer must not pile up unboundedly; at the cap a new `RaiseInteraction` is
  rejected and its gated action is denied with an `is_error` `ToolResult` (B6 — a rejected raise
  must not deadlock), applying backpressure to the agent.
- **Peer inbox** (`Caps.peer_inbox`, **RejectNewest**). An offline peer's inbox is capped; at the
  cap an inbound `SendPeer` yields `DeliveryOutcome::Rejected` to the sender (which becomes an
  `is_error` `ToolResult` for the driving `ToolUse`), so backpressure propagates to the SENDER
  rather than letting the receiver's inbox grow without bound.

### Bounded retries — backoff and restart intensity

Crash recovery re-dispatches outstanding effects (*Intent before commitment*); without a brake a
poison input could crash-loop forever. `Caps.restart` bounds it: each restart waits an
exponentially-backed-off delay (`base` doubling up to `ceiling`), and if MORE than `max_in_window`
restarts occur within `window` (restart intensity), the supervisor ESCALATES — gives up restarting
THIS process and surfaces it up (to a parent supervisor / the human) — instead of looping. The same
backoff governs `ModelFailed` retries.

### Large blobs — externalize to bound log and snapshot size

`Block::Image` and large `Block::ToolResult` payloads (screenshots, file dumps) would bloat the log
AND every snapshot if stored inline. Two stages:

- **v1 — inline / capped payloads, no separate store.** Blobs ride inline in the log up to a size
  cap; over the cap they are truncated/rejected. Simplest; no second artifact to keep durable.
- **v2 — content-addressed durable blob store.** Blobs are externalized to an **append-only,
  fsync'd blob store** keyed by the HASH of their bytes, and the block carries a small HASH
  reference, not the bytes. The store is **PRIMARY STORAGE, not a derived cache**: when the log
  records only a content hash, the blob BYTES are primary data — they are NOT regenerable from the
  log, so the store must be as durable as the log itself. Garbage collection is by
  **log-reachability**: a blob whose hash is no longer referenced by any retained log segment or
  snapshot may be reclaimed; one still reachable must never be. Content addressing still gives
  automatic dedup (one entry per hash) and tamper detection (a hash that resolves to different
  bytes is corruption, detected on read) — but this is single-source *of the bytes by hash*, NOT a
  cache the log can rebuild (see the corrected claim in *Single source of truth*, lens 8).

This stage bounds the SIZE carried by the log/snapshot; the durability and GC of the bytes are the
blob store's own responsibility, not the log's.

### Bounded mode window

`mode(edge)` folds over "recent" Events on the edge; "recent" is bounded at `Caps.mode_window` events
so the projection is O(window), not O(history).

---

## Single source of truth (the log is authoritative)

Event sourcing's core promise (Fowler): the same fact is recorded in exactly ONE authoritative
place; everything else is a **projection** of it. Two independent records of one fact inevitably
DRIFT, and there is no principled way to decide which is right. So this runtime keeps exactly one
authority and derives everything else from it:

**The per-App logical Input log (Stratum 1) is the single source of truth.** Snapshots are a
**cache** of the log (a materialized fast-forward), not a second source. Every other durable
artifact — `History`, `Components.raised` (`RaisedInteractions`), `TokenBudget`, the `WorldGate`/
`EntityGate` state, `Activity`, `Inbox`, `Resources.edges`, `IdAlloc`, and `mode(edge)` — is a
**projection rebuilt by replaying the log through the Systems**. None is an independent durable
store of the same fact. **On any conflict, the log wins**: a projection that disagrees with the
log is stale or corrupt, and is discarded and recomputed — never trusted over the log.

### `History` is a projection, not a second store

`History` is a **materialized projection** of the log — a cache of the block-structured
conversation kept so the next `CallModel` need not re-fold the whole log every turn. It is fully
reconstructable by replay: `ModelResponded.blocks` append assistant Msgs, `ToolReturned`/
`HumanActionDone` append the single `ToolResult` user Msg, the agent's `set_value` `ToolReturned`
(`origin = Agent`) updates the surface view (NO agent-origin `SurfaceMutated` is logged — Theme 1c),
and `Compacted` splices the `summary` over the `replaced` Msgs. It is NOT a parallel
source of truth.

The live code persists `session.jsonl` separately. **Canonical = the logical Input log;
`session.jsonl` is a derived VIEW** — a human-/tooling-readable export of the `History` projection,
regenerable from the log and never read back as authority. If `session.jsonl` and the log disagree,
the log wins and `session.jsonl` is rewritten. There is one conversation store of record, and it is
the log.

### `resume.json` is eliminated — restore state rebuilds from the log

Because the entire `World` is a **pure fold of the log**, all restore-relevant transient state is
reconstructable by replay and needs NO out-of-log record:

- **Pending interactions** (`Components.raised`): the ISSUANCE of a `RaiseInteraction` is re-emitted
  deterministically when its originating Input replays through `InteractionSystem`/`AutonomySystem`,
  so `raised` rebuilds from the log alone; its `InteractionAnswer` (an EXOGENOUS Input) clears it.
- **Pending human actions** (a `Human` slot in `ResolvingToolUses`): `RequestHumanAction` is
  effectful, so it also leaves a write-ahead `CommandDispatched` dispatch-intent; replay re-enters
  `ResolvingToolUses` with the pending `Human` slot and the *resume* phase reconciles the
  outstanding ask (*Intent before commitment*) — no side file.
- **Per-entity `Inbox`** and **undelivered-to-this-app peer messages**: the `Inbox` is World state
  (replayed through the Systems); inbound peer traffic is the logged `PeerDelivered`/`DriveRequested`
  stream, re-applied on replay.

So **`resume.json` shrinks to nothing** — every fact it once held already lives in the log. The peer
broker's OUTBOUND queue to an offline peer is the only state outside this App's World, and it is not
a third independent record either: each queued message is bound to a logged `PeerSendOutcome{Queued}`
on the sender and lands as a logged `PeerDelivered`/`DriveRequested` on the receiver, so the queue is
reconstructable from those two logs. If any residual runtime cache must still live outside the log, it
MUST be written **atomically (temp + fsync + rename)** and is explicitly a **cache, not truth** — the
log wins on conflict.

### Snapshots are a derived cache — prunable and regenerable

A snapshot is a **pure function of the log up to its tick**: `snapshot(t) = replay(log[..t])`. It
exists only to bound restore cost (*Boundedness and backpressure*); it records no fact the log lacks.
Therefore:

- **A snapshot serializes the ENTIRE `World` value** — `clock`, `root`, every entity's
  `Components`, and ALL of `Resources` including `Resources.ids` (`IdAlloc`), `Resources.rng`
  (`Rng`), and `Resources.wall` — so resume from a snapshot CONTINUES id-minting and RNG draws
  from the snapshot's state, never from a fresh allocator or reseeded PRNG. EQUIVALENCE: because
  the World is a pure fold of the log, the `IdAlloc`/`Rng`/`clock`/Component state captured in a
  snapshot at tick T equals the state a full replay reaches at tick T; therefore
  resume-from-snapshot and full-replay mint IDENTICAL ids and draw IDENTICAL randomness from T
  onward.
- **The log is authoritative.** If a snapshot and the log ever disagree (a snapshot is corrupt,
  truncated, or written by stale code), the snapshot is DISCARDED and regenerated by replay. Each
  snapshot stores `digest = hash(serialize(World))`; on restore the loader recomputes the digest
  of the deserialized World and REJECTS the snapshot if it does not match (corruption / bit-flip),
  falling back to an EARLIER snapshot (or genesis) + tail replay. Because the log remains
  authoritative, a rejected snapshot NEVER causes data loss — the World is simply rebuilt from the
  log.
- **Pruning rule.** Retain only the most recent **K** snapshots (K small — e.g. the latest 2–3 behind
  the live tail); older snapshots are deleted because they are REGENERABLE by replay from an earlier
  snapshot (or genesis) plus the log tail. The **log itself is never pruned below what is needed to
  regenerate** a retained snapshot's successors — cold-storing fully-covered segments is allowed,
  deleting them is not (that would destroy the source of truth).

### Per-call model/capability record is authoritative on replay; the live table is live-only

The `ModelMeta` recorded on `ModelResponded` (`model_id`, `capabilities`, `reasoning`) is the
**single source of truth on replay** — never the live routing/capability table. These are not two
records of one fact: the live table is consulted ONLY when DISPATCHING a live call, to PRODUCE the
record; once recorded, the per-call record SUPERSEDES the table for that call forever. On replay a
System reads `meta` and never re-resolves the table, so a table that has since changed (a remapped
alias, a new capability default, a different reasoning policy) cannot retroactively alter a recorded
trajectory. On conflict, the recorded `meta` wins.

### Content-addressed blobs are single-source by hash — but PRIMARY storage, not a derived cache

Large `Block::Image` / `Block::ToolResult` payloads (in the v2 store — *Boundedness*) live in a
**content-addressed blob store** keyed by the HASH of their bytes. Content addressing makes the store
single-source BY CONSTRUCTION at the level of IDENTITY: there is exactly ONE entry per content hash,
the log references a blob only by that hash, de-duplication is automatic (two identical blobs collapse
to one entry), and a hash that resolves to different bytes is corruption (detected on read), not a
competing version. **Correction (Codex-r2):** the blob BYTES are nevertheless **PRIMARY DATA, not a
derived/regenerable cache** — when the log stores only the hash, the bytes are NOT reproducible by
replaying the log, so the store is a SECOND primary durable artifact and must be as durable as the log
(append-only + fsync), with log-reachability GC. The log is single-source for *which* blobs exist (the
hash references); the store is single-source for the *bytes* of each hash. Neither can drift from the
other (the hash binds them), but neither regenerates the other: losing the store loses the bytes.

### The agent UI-write echo is one fact, recorded once

An agent UI write is a SINGLE fact whose **canonical and ONLY log record is the Effectful
`set_value` `RunTool`** — its `ToolUse` block (a `History` projection of `ModelResponded.blocks`),
its write-ahead `CommandDispatched`, and its `ToolReturned`. There is **NO agent-origin
`SurfaceMutated` log record at all**: the agent-origin surface change is a pure PROJECTION of that
`RunTool`'s `ToolReturned` (per *Correlate by identity*, carrying the originating RunTool's
`cmd`/`fingerprint`) onto the surface-view + mode-as-projection streams. `SurfaceSystem` folds it
**exactly once keyed by `cmd`** directly from the `ToolReturned` (never re-authored, never
double-applied to the surface view nor double-counted in the mode fold, which folds the `Event`
WRAPPING that `ToolReturned`, `origin = Agent`, never the `RunTool` Command directly — Theme 1d).
Only **human-origin** `SurfaceMutated` is a primary,
first-class logged Input: a genuinely EXOGENOUS fact with no originating effect, which is its own
canonical record. Canonical = the `set_value` `RunTool` effect for agent writes, the `SurfaceMutated`
Input itself for human writes.

### Dual-source audit

Every durable artifact reduces to **log + projection**; none is a second authority for a fact the log
already holds:

| Artifact | Status | Canonical source |
|---|---|---|
| `History` / `session.jsonl` | projection / derived view | logical log |
| `Components.raised`, `Activity`, `Inbox`, `TokenBudget`, gates, `edges`, `IdAlloc`, `mode` | projections (pure fold of the log) | logical log |
| Snapshots | derived cache (prunable, regenerable) | logical log |
| `resume.json` | ELIMINATED | logical log |
| Blob store (v2) | PRIMARY storage of the bytes (durable; single-source by hash; NOT a derived cache) | the bytes ARE the source; the log is canonical only for *which* hashes are referenced |
| Model/capability record | per-call record, authoritative on replay | `ModelResponded.meta` (table is live-only) |
| Agent surface change | projection of the `set_value` `RunTool` (NO separate log record) | the `RunTool`/`ToolReturned` effect |
| Broker peer-queue | reconstructable from `PeerSendOutcome` + `PeerDelivered` | the two Apps' logs |

No fact is recorded twice as two independent authorities; each non-log artifact is a projection, a
cache, or a derived view, and the log wins on every conflict.

---

## State machines

**Activity (per entity)** — Activity is turn PROGRESS only; whether a transition that INITIATES
or CONTINUES work may fire is a SEPARATE gating axis: every such transition requires `WorldGate ==
Open` (App-wide) AND that entity's `EntityGate == Open` (per-entity). A result-SETTLING transition
(recording a `ToolReturned`/`ChildReturned`/`HumanActionDone`/peer result / `ModelResponded` into
`History` + its slot) is NEVER gate-blocked — it always fires, and only the NEXT continuation is
deferred while gated (GateSystem; Invariant 13; *Tick discipline* phase 3).
Activity carries no gate state.

The table is TOTAL: every (state, input) has a defined cell — including the error, cancel,
timeout, refusal, denial and racing-result edges, so **no state is a sink**. Inputs that act
on gates or config — `Pause`, `Resume`, `ClearPolicyHalt`, `SetAutonomy` — and human-origin
`SurfaceMutated`, plus the surface/edge bookkeeping inputs `SurfaceObserved` and `EdgeBound`, are
orthogonal to Activity (they change Resources/UI, never the entity's
Activity) and are omitted from the rows; `DriveRequested`/`PeerSendOutcome` are handled by
PeerDriveSystem and touch Activity only via an enqueued prompt (→ behaves as a fresh
`UserMessage`) or by resolving a `Peer` slot (`PeerSendOutcome` → that slot's result). **Default
cell — applies to any (state, input) not listed below:** a defined no-op that re-logs the input
for trace and leaves Activity unchanged; for `UserMessage` arriving mid-turn the default is
"enqueue to Inbox" (steering), and for `Cancel` in a non-active state the default is "no-op + user
feedback". On crash recovery, *resume* synthesizes the tail Activity's terminating Input (e.g.
`InferenceCancelled{reason:Crash}` for `Thinking`, an `is_error` `ToolResult` for a non-idempotent
`Local` slot of a `ResolvingToolUses` turn) and feeds it through these same total edges — see
*Intent before commitment*.

| From | Input | → To | Emits / note |
|---|---|---|---|
| Idle | UserMessage | Thinking | CallModel (guard: admitted from Inbox by IntakeSystem; both gates Open) |
| Idle | UserMessage | Idle | — (guard: a gate Closed → stays queued in Inbox) |
| Idle | Cancel | Idle | — (no-op + feedback) |
| Thinking | ModelResponded · stop=EndTurn (text) | Idle | — |
| Thinking | ModelResponded · stop=ToolUse (N mixed tool_use blocks) | ResolvingToolUses (or Thinking/Idle if every slot resolves inline) | branch into N `ToolSlot`s by kind (`Local`/`Child`/`Human`/`Peer`), each tagged by `ordinal`; emit RunTool / spawn child / RequestHumanAction / SendPeer per slot; denied or inline slots → is_error result immediately |
| Thinking | ModelResponded · stop=Refusal | Idle | — (record refusal block + surface on human edge) |
| Thinking | ModelResponded · stop=MaxTokens | Thinking | CallModel (continuation; count capped by Boundedness) |
| Thinking | ModelResponded · stop=PauseTurn | Thinking | CallModel (continue server-tool turn) |
| Thinking | ModelFailed | Idle | — (record error + surface; retry = Boundedness) |
| Thinking | Cancel | Cancelling | CancelInference |
| Thinking | InferenceCancelled · reason=Crash (resume-synthesized) | Idle | — (record partial; crash's lost effect is now a stratum-1 Input; fresh retry decided by Autonomy/budget — *Intent before commitment*) |
| Thinking | ModelResponded · spend `used ≥ limit` (BudgetSystem) | Idle | EntityGate→Halted{BudgetExhausted}; is_error into any open slot — settle, not deadlock (*Boundedness*) |
| Thinking | ModelResponded · context `used` near `context_limit`, continuation pending | Compacting | Compact (defer the continuation CallModel) |
| Compacting | Compacted | Thinking | CallModel (deferred continuation; History summarized, context shrunk) |
| Compacting | ModelFailed (compaction failed) | Thinking | CallModel (proceed un-compacted, best-effort) |
| Compacting | Cancel | Cancelling | CancelInference |
| Compacting | InferenceCancelled (clears its cmd) | Cancelling / Idle | — (compaction aborted; → Idle when `awaiting` empty) |
| ResolvingToolUses | ToolReturned / ChildReturned / HumanActionDone·Provided / PeerSendOutcome (a slot resolves; others still Pending) | ResolvingToolUses | — (record result into its slot by `tool_use_id`/`cmd`/`(child,tool_use_id)`; slot→Done) |
| ResolvingToolUses | HumanActionDone · Declined/Timeout (a Human slot resolves) | ResolvingToolUses | — (is_error ToolResult into the slot; slot→Done) |
| ResolvingToolUses | ChildReturned{is_error} | ResolvingToolUses (or last → Thinking) | guard: terminal child (crash w/o recovery / EntityGate Halted) — SupervisionSystem synthesizes the is_error ToolResult; resolve that Child slot by `(child,tool_use_id)`; slot→Done (Totality) |
| ResolvingToolUses | ToolReturned / ChildReturned / HumanActionDone / PeerSendOutcome (the terminal slot result) | Thinking | guard: last slot Done (across ALL kinds) — CallModel, ONE user Msg, all slot results in ASCENDING `ordinal` order (continuation DEFERRED while a gate is Closed — GateSystem; Invariant 13) |
| ResolvingToolUses | ToolReturned / ChildReturned / HumanActionDone / PeerSendOutcome (the terminal slot result) | Thinking | guard: last slot Done · `turns ≥ loop_cap` — FINAL wrap-up CallModel, tools WITHHELD, inject is_error "step budget exhausted; finalize now"; the model's text answer then → Idle (*Boundedness*) |
| ResolvingToolUses | InteractionAnswer · approve (gated tool) | ResolvingToolUses | RunTool (the held tool) |
| ResolvingToolUses | InteractionAnswer · reject (gated tool) | ResolvingToolUses / Thinking | resolve that slot with is_error; → Thinking if it was the last slot |
| ResolvingToolUses | ModelResponded | Idle | guard: spend `used ≥ limit` → EntityGate→Halted{BudgetExhausted} (BudgetSystem, mid-resolve) — is_error into every Pending slot, settle not deadlock (*Boundedness*) |
| ResolvingToolUses | Cancel | Cancelling | cancel every Pending slot: CancelTool (Local) / AbortHumanAction (Human) / Cancel→children (Child); a Peer slot's durable send is not recallable — stop awaiting it and record its owed PeerSendOutcome in `awaiting`, absorbed on arrival |
| Cancelling | InferenceCancelled (clears its cmd) | Cancelling / Idle | — (keep partial; → Idle when `awaiting` empty) |
| Cancelling | ToolAborted (clears its cmd) | Cancelling / Idle | — (→ Idle when `awaiting` empty) |
| Cancelling | HumanActionAborted (clears its cmd) | Cancelling / Idle | — (→ Idle when `awaiting` empty) |
| Cancelling | racing real result (ModelResponded / ToolReturned / ChildReturned / HumanActionDone / PeerSendOutcome / Compacted / compaction ModelFailed) | Cancelling / Idle | — (absorbed as that cmd's/slot's owed ack; slot/cmd settles, then → Idle when `awaiting` empty) |
| Cancelling | Cancel | Cancelling | — (already cancelling) |

**WorldGate (App run-state)** — `Open ↔ Paused { holds }`, where `holds` is a SET of
concurrent `PauseHold`s; Paused is STICKY (a message queues, does not auto-resume). Clearing
is total and PER-SOURCE — a `User` resume cannot clear a `PolicyHalt` and vice-versa:

| From | Input | → To |
|---|---|---|
| Open | Pause{User} / Pause{PolicyHalt(trip)} | Paused{holds = {that hold}} |
| Paused | Pause{…} | Paused{holds ∪ that hold} (idempotent per source) |
| Paused | Resume | remove ALL `User` holds; `Open` iff `holds` now empty, else Paused |
| Paused | ClearPolicyHalt{trip, authority} | remove the matching `PolicyHalt(trip)` hold (authority required); `Open` iff `holds` now empty, else Paused |
| Open | Resume / ClearPolicyHalt | Open (no-op) |

The gate reopens ONLY when `holds` is empty. (Authority VERIFICATION — who may issue a
`ClearPolicyHalt` — is an enforcement concern for a later pass; the totality requirement is
that the clearing transition EXISTS and is gated by an authority token.)

**EntityGate (per-entity run-gate)** — `Open ↔ Halted { reason }`, the per-entity analog of the
WorldGate's PolicyHalt axis. A budget halt (`Halted { BudgetExhausted }`) is set by BudgetSystem; its
ONLY clearing input is `ClearEntityHalt` (the per-entity analog of `ClearPolicyHalt`, owned by
BudgetSystem), so a budget-halted entity is never a permanent sink:

| From | Input | → To | Emits / note |
|---|---|---|---|
| Halted (entity, budget) | ClearEntityHalt{entity, authority} | Open | reopen the entity's EntityGate (authority required); enforcement deferred |

As with `ClearPolicyHalt`, authority VERIFICATION — who may issue a `ClearEntityHalt` — is a
later-pass enforcement concern; the totality requirement is that the clearing edge EXISTS and is
gated by an authority token, so the budget-halt sink is now NAMED, not silent.

**Residency (supervisor concern, NOT a World Component)** — `Resident |
Tombstoned`. A Tombstoned app AUTO-restores transparently on the next message;
"transparent" = behaviorally-neutral + engine-driven-and-traced +
experientially-seamless. Restore loads the nearest **snapshot ≤ target tick** and replays only the
**tail** of the logical log after it (NOT the whole log — see *Boundedness and backpressure*);
emits a `Restored` lifecycle event; `WorldGate` is preserved across restore. A Tombstone is a CLEAN
suspension (no in-flight effects), so its restore is the **replay** phase only; a CRASH recovery
additionally runs the **resume** phase to reconcile outstanding dispatch-intents (see *Intent
before commitment*) — and because resume only inspects the replayed tail, crash reconciliation is
bounded to the recent tail too. How many App processes stay `Resident` at once is itself a bounded
soft cap with an admission/eviction policy (idle-only LRU; never evict an app mid-turn) — see
*Boundedness and backpressure*.

## Mode-as-projection

Mode is NOT stored. It is a **pure fold over the `origin` of recent Events on an
`edge`** — both now STRUCTURAL on the `Event` envelope, so the fold is computable
rather than asserted. Only **actor** samples count; **neutral** origins are ignored:

```
mode(edge):
  ACTORS  = { e.origin | e ∈ recent log, e.edge == edge, e.origin is an ACTOR }
  NEUTRAL = System; and the neutral inputs SessionStarted, Timer, Compacted,
            crash-synthesized InferenceCancelled{reason:Crash}, and policy events
            — these NEVER count toward any mode (Theme 3b)
  no actor samples (empty ACTORS)        → Operating   (the explicit empty-window result, Theme 3a)
  ACTORS == {Human}                      → Operating
  Human interleaved with Agent           → Assisted     (requires BOTH present, Theme 3b)
  at least one Agent or Peer, no Human   → Driven        (requires an Agent/Peer actor sample;
                                                          NEVER inferred from mere absence of Human)
```

Because each edge is classified INDEPENDENTLY, the modes coexist: a lead agent's
human edge folds to Assisted while its app-driving edges fold to Driven at the same
time. The "recent" window is BOUNDED at `Caps.mode_window` events, so the mode fold is
O(window) not O(history) (*Boundedness and backpressure*).

**Concurrent per-edge work (Theme 3c).** Mode concurrency — Assisted on one edge, Driven on
another, simultaneously — is a PROJECTION and is fully supported by this per-edge fold. But each
entity has a SINGLE `Activity`, so genuinely concurrent independent WORK on two edges is NOT modeled
by parallel turns inside one entity; it is modeled by edge-scoped SUBAGENTS — each a homogeneous
`AgentLoop` entity with its OWN `Activity` (and its own edge). This is the deliberate resolution:
the runtime does NOT add a `Map<EdgeId, Activity>` to `Components`; one entity = one turn, and
parallelism comes from spawning entities, not from multiplexing one entity's Activity across edges.

## Anthropic model-call mapping

`MessageBuilder` serializes a `CallModel` to `POST /v1/messages`: assembled system
→ top-level `system`; `History` blocks → `messages[blocks]`; `ToolUse` in an
assistant message; `ToolResult` in a USER message; `Reasoning` → `thinking`
(echoed unchanged with signature); `ToolSet` → `tools[{name,description,input_schema}]`;
`ModelConfig` → `model`/`max_tokens`/`thinking:adaptive`/`output_config.effort`.
Stateless. UI-manipulation tools ride the standard `ToolUse` path; UI state /
screenshots return as `ToolReturned` (may include an `Image` block).
Any **date/time the assembled `system` embeds** (e.g. "today is …") is read from
`Resources.wall` (fed from the log), NEVER `now()`, so the serialized request is
identical on replay. The **effective model id, capability metadata, and reasoning
round-trip policy** the shell resolves for the call are reported back in
`ModelResponded.meta` (recorded), so a replay reconstructs the same request and the
same branch without consulting any live routing/capability table.

## Differentiators (UI + peer)

- **UI manipulation** is a set of tools (`render_component`, `set_value`, `click`,
  `navigate`, the whiteboard interaction vocabulary), each naming what it manipulates via a
  payload-bearing `SurfaceOp` (`SetValue`/`Click`/`Navigate`/`Render` with an optional
  `base_version` precondition — Theme 2a) and perceiving the UI it acts on via `SurfaceObserved`
  (Theme 2b). The agent's UI actions are `ToolUse`
  blocks executed by the shell against the native client; each executed write's log record is its
  `set_value` `RunTool` (`ToolUse` + `CommandDispatched` + `ToolReturned`), and the surface-view +
  mode change for an agent write is a PROJECTION of that `ToolReturned` (no separate
  `SurfaceMutated` record). A human's direct edit is the one logged `SurfaceMutated` (origin
  Human; a `Command`/`Peer`-caused native echo is deduped by `cause` — Theme 1e) — so the log is
  the single source of UI truth and a human grabbing the wheel stays in sync (B8).
- **Peer / cross-app.** `PeerDelivered` carries an inbound peer message; a lead agent drives
  a peer via a structured `DriveCommand` (payload-bearing `surface_ops`) carried in
  `PeerPayload::Drive` over `SendPeer`, executed by the target's shell
  against its client and recorded in the target's log as `DriveRequested` — bound to the receiver's
  `edge_for(Counterpart::Peer(from))` (Theme 4a), with the sender's
  authorization, since this lets A act inside B; the sender records `PeerSendOutcome` (its only
  dual — Theme 4b). An
  offline target queues to an inbox, delivered on restore. The broker is a switch.

---

## Decisions (ADR-style)

- **D1 — Pure event-sourced World, not an in-process loop.** Enables deterministic
  replay-for-evaluation; the alternative (the current scattered `AgentLoop`) is
  not serializable. Rejected: bolting record/replay onto the existing loop.
- **D2 — ECS discipline without a game ECS library.** We adopt the discipline
  (state-as-data, pure Systems, explicit step) but not a tick-rate runtime;
  rejected: importing `bevy_ecs` (async-tick impedance mismatch at this scale).
- **D3 — Anthropic Messages format first.** Agent-native block content + the
  open-weight vendors' Anthropic-compatible endpoints; canonical IR ≈ Anthropic,
  so the adapter is near 1:1. A Chat-Completions adapter is a later downcast.

---

## Invariants

The cross-cutting invariants the eight hardening passes established, consolidated as a checklist a
linter (or reviewer) could enforce. Each is mechanical enough to flag a violation in code review:

1. **Functional-core purity.** Every System is a pure reducer `(World, Input) → (World', Vec<Command>)`;
   its ONLY inputs are the handed `World` and the one `Input`. No ambient reads — no host clock, no
   ambient RNG, no env/config, no live capability table, no exposed-`HashMap` iteration. *(Pass 4.)*
2. **Logical time = tick.** `clock: Tick` is the sole ORDERING axis; observed wall-time
   (`Resources.wall`, primed from `Event.wall`) is a DATUM the agent may read, never the ordering axis
   and never `now()`. *(Pass 2.)*
3. **All non-determinism is recorded.** Every hidden input — model/tool/human/peer result, wall-time,
   the RNG seed, the effective `model_id`/`capabilities`/`reasoning` — crosses a RECORDED boundary (a
   logged Input and/or a Resource fed only from the log) before any System reads it. *(Pass 4.)*
4. **Log-before-apply.** No World mutation is applied before its causing Input is DURABLY appended to
   the stratum-1 log, so the World stays a pure function of the durable log. *(Pass 5.)*
5. **Intent before commitment (WAL).** Each effectful Command appends a write-ahead `CommandDispatched`
   (intent + `key` + `fingerprint`) BEFORE it acts; `effectively-once = at-least-once + idempotency`;
   the *resume* phase reconciles every outstanding dispatch-intent. *(Pass 5.)*
6. **Replay-suppress vs resume-redispatch.** The replay driver DISCARDS every emitted Command (logged
   results stand in); the resume phase RE-DISPATCHES (or settles) only outstanding dispatch-intents in
   the replayed tail. A System cannot tell which driver runs it. *(Passes 4–5.)*
7. **Content-addressed replay.** Every DERIVED *effect-result* is correlated to its request by a
   canonical `fingerprint` (+ `cmd`), never by log position; on re-emit a recorded result is reused
   ONLY on fingerprint match, else that branch goes LIVE — EXCEPT the abort acks
   (`ToolAborted`/`HumanActionAborted`) and the in-World derived input `ChildReturned`, which are
   IDENTITY/`cmd`-keyed and deliberately NON-fingerprinted (Theme 5a). EXOGENOUS Inputs are free
   variables, carried verbatim and never fingerprinted. *(Pass 6; Codex r4.)*
8. **Deterministic ids & iteration.** `cmd`/`request_id`/`edge_id`/`timer_id`/`effect_id` are PURE
   functions of World state (sole minter `Resources.ids`), never shell-assigned; every `Map<…>` is
   deterministically ordered (no exposed `HashMap` iteration in a System). *(Pass 6.)*
9. **Single source of truth.** The logical Input log (+ snapshots as a cache) is AUTHORITATIVE;
   `History`, gates, budgets, `raised`, `mode`, etc. are projections; no fact is recorded twice as two
   authorities; the log wins on conflict. *(Pass 8.)*
10. **Totality.** Every (state, Input) cell is defined; no waiting state is a SINK — every denial,
    error, timeout, refusal, abort, or halt synthesizes a terminating Input (an `is_error` `ToolResult`
    into the originating `ToolUse`, or a settling transition). *(Pass 3.)*
11. **Boundedness.** Every loop, queue, growing structure, and retry carries an explicit brake
    (`loop_cap`, `fanout_cap`, `Caps`, `TokenBudget`, snapshots, `RestartPolicy`) — all World state, so
    the brakes replay deterministically like everything else. *(Pass 7.)*
12. **Fixed intra-tick System order.** Exactly one Input is processed per tick, and the Systems run in
    a FIXED PHASE ORDER (lifecycle/gates → intake → settle results → budget/compaction → turn-advance →
    emit Commands). `effect_id` and command order are assigned ONLY from the final, fixed Command list —
    deterministic, never scheduler-dependent. *(Codex r2; *Tick discipline*.)*
13. **Gates block new work, never result-settling.** A Paused `WorldGate` / `EntityGate::Halted` blocks
    turn INITIATION and CONTINUATION only; a terminal result already in flight
    (`ModelResponded`/`ToolReturned`/`ChildReturned`/`HumanActionDone`/peer result) is ALWAYS folded
    into `History` + its slot, and only the NEXT continuation is deferred until the gate reopens.
    *(Codex r2; GateSystem.)*
14. **Cancellation absorbs every owed ack.** Every active state's outstanding `cmd`/slot — model, tool,
    child, human, peer, and compaction — has an owed ack recorded in `Cancelling.awaiting`, IDENTIFIED
    by the slot's `Pending { cmd: Some(cmd) }` (`Local`/`Human`/`Peer`) or its `(child, tool_use_id)`
    (`Child`, `Pending { cmd: None }`), that is absorbable (the abort ack OR the racing real result,
    including `PeerSendOutcome`/`Compacted`/compaction `ModelFailed`), so `Cancelling → Idle` always
    reaches an empty `awaiting`; a `Peer` slot's durable send is NOT recallable, so its
    `PeerSendOutcome` is recorded and absorbed (result discarded) on arrival. *(Codex r3/r4; CancelSystem.)*
15. **Every Halted gate has a named clearing input.** Each halt has an EXOGENOUS, authority-gated
    clearing Input: a `WorldGate` `PolicyHalt` clears via `ClearPolicyHalt`, and an
    `EntityGate::Halted` (e.g. budget exhaustion) clears via `ClearEntityHalt` (BudgetSystem reopens).
    No halt is a permanent sink. *(Codex r3; GateSystem / BudgetSystem.)*
16. **Every pending slot maps to its result by identity.** Each `Pending` `ToolSlot` carries the
    identity by which its terminal result is matched back: a `Local`/`Human`/`Peer` slot holds
    `Some(cmd)` (resolved by `ToolReturned`/`HumanActionDone`/`PeerSendOutcome` on `cmd`), a `Child`
    slot holds `None` (resolved by `ChildReturned` on `(child, tool_use_id)`) — never by arrival
    order. *(Codex r4; Theme 1a.)*
17. **Every result inherits `ctx` from its dispatch.** Every entity-scoped Command is dispatched with
    an `ActorCtx { entity, origin, edge }` recorded on `CommandDispatched.ctx` (the durable `cmd → ctx`
    index); every result Input inherits its `entity`/`origin`/`edge` from the matching
    `CommandDispatched.ctx`, so result routing has a durable source and a derived UI effect stays
    causally attributable. *(Codex r4; Theme 1b.)*
18. **UI tools fingerprint perceived surface state.** A UI / `set_value` tool is External-state: its
    `Fingerprint` MUST fold in the perceived `surface_version`/`ax_digest` (recorded by
    `SurfaceObserved`), so the macOS UI state it depended on crosses a recorded boundary and is not a
    hidden replay input. The agent UI write's SOLE log record is its `set_value` `ToolReturned`
    (`origin = Agent`); NO agent-origin `SurfaceMutated` is ever logged, and a `Command`/`Peer`-caused
    native echo is dropped by `cause`. *(Codex r4; Themes 1c/1e/2b.)*
19. **Mode never infers Driven from absence.** `mode(edge)` counts only ACTOR origins; `System` and
    the neutral inputs (`SessionStarted`/`Timer`/`Compacted`/crash-synthesized `InferenceCancelled`/
    policy events) are MODE-NEUTRAL. `Driven` requires at least one `Agent`/`Peer` actor sample and
    `Assisted` requires interleaved Human+Agent; an empty actor window folds to `Operating`. Driven is
    never inferred from the mere absence of Human. *(Codex r4; Themes 3a/3b.)*

---

## Codex review (round 1) — applied fixes

An independent Codex review of the hardened schema surfaced six gaps; each is now addressed in
place (above), with no change to the surrounding model:

1. **B6 child terminal-failure → parent `is_error` (the one partially-closed blocker).** A child
   Entity that reaches a TERMINAL non-viable state (crash-reconciliation with no recovery, or an
   `EntityGate::Halted` that cannot proceed) is detected by a **SupervisionSystem** (or the
   explicit rule in `SubagentSystem`), which removes it from the parent's
   `AwaitingSubagents.children` and synthesizes an `is_error` `ToolResult` (with the failure
   reason) into its `ToolUse` slot. The parent advances once `children` is empty exactly like the
   success path, so a dead child can never leave the parent waiting forever — reconciled with the
   TOTALITY INVARIANT. (Named in *Systems → SubagentSystem*; new Activity-table row.)
2. **One-way `replay → live` handoff.** Made the driver handoff explicit and unambiguous: a run
   holds a SINGLE mode that flips EXACTLY ONCE — it starts in replay and, on the FIRST fingerprint
   mismatch, transitions one-way to live at that point; every subsequent Command on that branch is
   live, with no return to replay and no parallel replay+live. This first-mismatch boundary is the
   fork/edit boundary. (*Determinism discipline*; *Correlate by identity → content-addressed
   replay*.)
3. **Snapshot completeness on resume / `IdAlloc`.** A snapshot serializes the ENTIRE `World`,
   including `Resources.ids` (`IdAlloc`), `Rng`, `clock`, and every Component, so resume continues
   id-minting and RNG from the snapshot's state; and because the World is a pure fold of the log,
   resume-from-snapshot and full-replay mint identical ids and draw identical randomness from the
   snapshot tick onward. (*Single source of truth → Snapshots are a derived cache*.)
4. **Snapshot validation by content hash.** Each snapshot stores `digest = hash(serialize(World))`;
   on restore the loader recomputes the digest and rejects a snapshot whose digest does not match,
   falling back to an earlier snapshot (or genesis) + tail replay. The log stays authoritative, so
   a rejected snapshot never causes data loss. (*Single source of truth → Snapshots are a derived
   cache*.)
5. **Edit scope + mechanical purity.** Edits target EXOGENOUS inputs only (`UserMessage`,
   `Pause`/`Resume`/`Cancel`, `InteractionAnswer`, `SetAutonomy`, `PeerDelivered`, human-origin
   `SurfaceMutated`); DERIVED inputs are never edited in place — rewind and let the call re-run
   live. And the "pure reducer of `(World, Input)`" invariant is enforced mechanically (Systems
   take `&World`/`&Input`; a `cargo xtask lint`-style check bans `SystemTime::now()`/`rand::`/
   `std::env`/exposed-`HashMap` iteration). (*Correlate by identity*; *Determinism discipline*.)
6. **Model output contract.** The agent/World consumes the assembled canonical `blocks` only; raw
   streamed tokens are a shell concern and never a System input (streaming is display-only), with
   exactly one `ModelResponded` per inference as the System-visible result. (Defined at
   `ModelResponded`.)

APPLIED (Codex r1): B6 child-failure signaling · one-way replay→live handoff · snapshot
completeness (`IdAlloc`/`Rng`/`clock`/Components) · snapshot content-hash validation · edit-scope +
mechanical purity enforcement · model-output (`blocks`-only) contract — all addressed in place; no
other content altered.

---

## Codex review (round 2) — applied fixes

A second independent Codex review surfaced eight correctness gaps; each is now addressed in place
(above). Where round 2 UNIFIES prior-pass states, the earlier `UsingTools`/`AwaitingSubagents`/
`AwaitingHuman` model (recorded in *Codex review (round 1)* and the Appendix lenses as the state of
those passes) is SUPERSEDED by the single `ResolvingToolUses` slot model below — those retrospective
logs describe the pre-r2 shape they changed.

1. **Fixed intra-tick System order (CRITICAL).** Added a *Tick discipline* statement: one Input per
   tick, a FIXED PHASE ORDER (lifecycle/gates → intake → settle terminal results → budget/compaction
   → turn-advance → emit Commands), with `effect_id`/command order assigned ONLY from the final
   Command list — deterministic, not scheduler-dependent. Published the canonical schedule and
   referenced it from *Systems* and the new Invariant 12. (*Systems → Tick discipline*.)
2. **Unified tool-resolution into ordered slots (CRITICAL + HIGH).** Replaced `Activity::UsingTools`
   and `Activity::AwaitingSubagents` and folded the tool-driven `AwaitingHuman` into ONE state
   `ResolvingToolUses { slots: Vec<ToolSlot> }` (`ToolSlot { tool_use_id, ordinal, kind, state,
   result }`, `SlotKind { Local, Child(EntityId), Human, Peer }`, `SlotState { Pending, Done }`).
   `ordinal` = the assistant block order (not arrival). One assistant turn now MIXES local tools +
   sub-agent delegations + human actions + peer drives; each slot resolves independently and, when
   ALL are `Done`, the turn assembles EXACTLY ONE user message with `tool_result` blocks in ORDINAL
   order. Updated TurnSystem (branch into N mixed-kind slots), ToolSystem/SubagentSystem/
   HumanActionSystem/PeerDriveSystem (resolve their slot kind), SupervisionSystem (child terminal
   failure resolves its `Child` slot with an `is_error` result), `Cancelling` (cancels all `Pending`
   slots), and the Activity transition table (slot-resolution rows + one "all slots Done → assemble
   single user msg → continue" row). A purely-human turn is one `Human` slot.
3. **Child result as an explicit correlated input (HIGH).** Added the DERIVED `ChildReturned {
   parent, child, tool_use_id, result }` so a child's completion is a logged, correlated event
   (by `child`+`tool_use_id`) resolving the parent's `Child` slot — consistent with peers/tools —
   instead of an implicit in-World fold whose timing depended on System order.
4. **Gate blocks new work, never result-settling (HIGH).** Amended GateSystem and the gate prose: a
   Paused `WorldGate`/`EntityGate::Halted` blocks turn INITIATION and CONTINUATION only; terminal
   results in flight are ALWAYS settled into `History`/slots (tick phase 3), with the next
   continuation deferred until the gate reopens. Removes the "paused-while-resolving swallows the
   result / entity stuck" bug. (New Invariant 13.)
5. **`SurfaceMutated` coherence (HIGH).** Made it consistent: the ONLY logged `SurfaceMutated` Input
   is HUMAN-origin (primary EXOGENOUS). Agent UI writes are the `set_value` `RunTool` (`ToolUse` +
   `CommandDispatched` + `ToolReturned` — the single commit record); the agent-origin surface change
   is a PROJECTION of that `ToolReturned`, NOT an independent stratum-1 input. Updated the type, the
   EXOGENOUS/DERIVED classification, SurfaceSystem/PeerDriveSystem, and the single-source/UI-echo
   prose so there is no contradiction.
6. **Durable peer outbox/inbox protocol (HIGH).** Specified it: the broker fsyncs the envelope; the
   receiver idempotently appends `DriveRequested`/`PeerDelivered` keyed by envelope id; the sender
   records `PeerSendOutcome::Delivered` ONLY after a receiver durable ack. Model: at-least-once +
   idempotency-by-envelope-id (effectively-once), no atomic cross-log transaction — a crash leaves at
   worst a retried, deduped delivery, never disagreeing logs. (*Intent before commitment → Durable
   peer delivery*.)
7. **Fingerprint vs external state (HIGH).** Refined content-addressed replay: a fingerprint covers
   the REQUEST only, but a tool/peer result can depend on EXTERNAL state. Added a per-command REPLAY
   POLICY — Pure/Deterministic reuses the recorded result; External-state either folds a state-version
   token (file digest, surface version, peer-log head, auth epoch, ETag, as-of timestamp) into the
   fingerprint or is marked force-live-after-divergence — tied to `ToolEffect{Observational|Effectful}`.
8. **Blob store is primary storage, not derived (MEDIUM).** Corrected the single-source claim: with
   only content hashes in the log, the blob BYTES are primary data (not regenerable from the log). v1
   = inline/capped payloads (no store); v2 = an append-only DURABLE blob store (fsync) referenced by
   hash with log-reachability GC — primary storage, not a derived cache. Fixed the earlier "blobs are
   derived" wording in *Boundedness* and *Single source of truth*.

APPLIED (Codex r2): fixed intra-tick System order · `ResolvingToolUses` ordered-slot unification of
tools/subagents/human/peer · explicit `ChildReturned` correlated input · gates block new work not
result-settling · `SurfaceMutated` human-only-input / agent-projection coherence · durable peer
outbox/inbox (effectively-once by envelope id) · fingerprint-vs-external-state replay policy · blob
store as primary storage (v1 inline / v2 durable) — all addressed in place; the unified-away
`UsingTools`/`AwaitingSubagents`/`AwaitingHuman` states have no remaining live references.

---

## Codex review (round 3) — applied fixes

A third independent Codex totality audit (state-machine lens) surfaced five genuinely-open gaps;
each is now closed in place, with no change to the surrounding model. In particular the Activity
transition table is UNCHANGED except the extended `Cancelling` absorber input list and the one new
`EntityGate` row:

1. **`Cancelling` absorber total across ALL slot kinds + compaction.** The racing-result row now
   absorbs `PeerSendOutcome` and `Compacted`/compaction `ModelFailed` in addition to
   `ModelResponded`/`ToolReturned`/`ChildReturned`/`HumanActionDone`, and CancelSystem records those
   owed acks in `awaiting` — so cancelling while a `Peer` slot or a `Compacting` turn is in flight
   still settles to `Idle` (Invariant 14).
2. **Undefined `peer-cancel` resolved.** A `Peer` slot's durable send already sits in the
   at-least-once outbox and is NOT recallable; the command-implying "peer-cancel" wording (state
   comment, CancelSystem, the `ResolvingToolUses → Cancelling` row) is replaced by the honest model —
   stop awaiting the slot and record its owed `PeerSendOutcome` in `awaiting`, absorbed (result
   discarded) on arrival. No `CancelPeer` Command is invented; the real cancel Commands remain
   `CancelInference` / `CancelTool` / `AbortHumanAction` / `Cancel` (child-cascade).
3. **`EntityGate::Halted` clearing input named.** A new EXOGENOUS, authority-gated
   `ClearEntityHalt { entity, authority }` (the per-entity analog of `ClearPolicyHalt`) reopens a
   budget-halted `EntityGate::Halted → Open` (BudgetSystem owns the reopen) — so a budget-halted
   entity is no longer a permanent sink (Invariant 15). Enforcement deferred; the totality edge is
   named.
4. **`Inbox` DropOldest made observable.** A DropOldest eviction at the cap emits an observable drop
   notice on the human edge — a STRATUM-2 lifecycle record (trace only), NOT a silent loss and NOT a
   replay-defining stratum-1 Logical Input (the two-strata convention).
5. **Model retry classified.** A TRANSIENT `ModelError` (timeout / overload / 5xx / transport) is
   eligible for a bounded, accounted auto-retry — a FRESH `CallModel` (new `cmd`, new `key`) gated by
   Autonomy and remaining budget under `Caps.restart` backoff + give-up; a TERMINAL `ModelError`
   (auth / 4xx / invalid request / refusal-as-error) is surfaced on the human edge with NO auto-retry,
   consistent with the crash-retry rule.

APPLIED (Codex r3): `Cancelling` absorbs peer + compaction owed acks · honest non-recallable
peer-cancel (no `CancelPeer` Command) · `ClearEntityHalt` names the budget-halt clearing edge ·
`Inbox` DropOldest emits a stratum-2 drop notice · transient-vs-terminal model-retry classification —
all addressed in place; the Activity transition table is unchanged apart from the extended
`Cancelling` input list and the one new `EntityGate` row.

---

## Codex review (round 4) — applied fixes

Two independent Codex reviews (a cross-section COHERENCE pass and a PRODUCT-DIFFERENTIATOR pass)
converged on the same core findings; each is now closed in place, organized in five themes. The
earlier model is amended, not rewritten — the round-1/2/3 logs and the Appendix lenses stay as
history.

1. **Causal & routing context (root fix).**
   - **1a — Slot ↔ command identity.** `SlotState::Pending` now carries `Pending { cmd: Option<CmdId> }`:
     `Local`/`Human`/`Peer` slots hold `Some(cmd)` (resolved by `ToolReturned`/`HumanActionDone`/
     `PeerSendOutcome` on `cmd`), a `Child` slot holds `None` (resolved by `ChildReturned` on
     `(child, tool_use_id)`). ToolSystem/HumanActionSystem/PeerDriveSystem/CancelSystem and Invariant
     14 updated; new Invariant 16.
   - **1b — `ActorCtx` on dispatch.** Added `struct ActorCtx { entity, origin, edge }` and `ctx:
     ActorCtx` on `CommandDispatched`; every result Input inherits its `entity`/`origin`/`edge` from
     the matching `CommandDispatched.ctx` (the durable `cmd → ctx` index) — fixing result entity-routing
     (Coherence) AND derived-UI causal attribution (Differentiators). New Invariant 17.
   - **1c — Agent UI write = `ToolReturned`, the SOLE record.** Scrubbed the "`SurfaceMutated{origin:
     Agent}` is the commit record / updates history" wording: the agent UI write is the `set_value`
     `RunTool`; its `ToolReturned` (an `Event` with `origin = Agent`, `edge` from `ctx`) is the sole
     commit record, and NO agent-origin `SurfaceMutated` is ever logged.
   - **1d — Mode folds Events, not Commands.** Mode folds the `Event` wrapping the agent's
     `ToolReturned` (`origin = Agent`), human `SurfaceMutated` (`origin = Human`), and peer
     `DriveRequested` (`origin = Peer`); `RunTool` is a Command and is never folded directly.
   - **1e — Native echo dedup by `cause`.** Native UI signals carry `cause: Cause { Human | Command |
     Peer }`; only `cause = Human` is logged as a stratum-1 `SurfaceMutated`, a `Command`/`Peer`-caused
     signal is the echo of an already-logged `ToolReturned`/`DriveRequested` and is dropped.

2. **UI-manipulation expressiveness + UI replay.**
   - **2a — Payload-bearing `SurfaceOp`.** `SetValue`/`Click`/`Navigate`/`Render` now carry their
     `surface`/`element`/`value`/`route`/`component` and an optional `base_version` precondition;
     used by `DriveCommand.surface_ops`, the `set_value`/UI `RunTool`, and `SurfaceMutated`.
   - **2b — `SurfaceObserved` perception input.** Added the EXOGENOUS `SurfaceObserved`; UI tools'
     fingerprints fold in `surface_version`/`ax_digest` (round-2 External-state, stance (a)), closing
     the "macOS UI state is a hidden input" replay hole. New Invariant 18.
   - **2c — `SurfaceVersion` + conflict policy.** Per-surface/element `SurfaceVersion`; batched ops
     apply in `Vec` order, default `LastWriterWins`, an op with a stale `base_version` is rejected
     `is_error` (optimistic concurrency). Stated in SurfaceSystem.

3. **Mode classifier correctness.**
   - **3a** empty actor window → `Operating` (explicit). **3b** `System` and neutral inputs are
     MODE-NEUTRAL; `Driven` requires an `Agent`/`Peer` actor sample, `Assisted` requires interleaved
     Human+Agent; Driven is never inferred from mere absence of Human. **3c** concurrent independent
     WORK on two edges is edge-scoped SUBAGENTS, not a `Map<EdgeId, Activity>`. New Invariant 19.

4. **Cross-World peer correctness.**
   - **4a — Receiver edge binding.** Deterministic `edge_for(Counterpart) -> EdgeId` keyed by the
     durable `Counterpart`, with `EdgeBound { edge, counterpart }` logged the first time an edge is
     created; inbound `DriveRequested`/`PeerDelivered` bind to that edge.
   - **4b — De-conflated `SendPeer` duals.** `SendPeer ↔ PeerSendOutcome` is the sender's local ack
     (the ONLY dual); receiver-side `DriveRequested`/`PeerDelivered` are EXOGENOUS envelope-delivery
     inputs correlated by `PeerEnvelopeId`, not duals. `SendPeer.payload: PeerPayload { Drive | Message }`.

5. **Consistency cleanups.** `SessionStarted` added to EXOGENOUS; `ToolAborted`/`HumanActionAborted`
   added to DERIVED as `cmd`-keyed non-fingerprinted abort acks; Invariant 7 amended. `ChildReturned`
   named as a deterministic in-World exception to the dual rule. `Compact` added to the WAL key list +
   a `Compacting (Compact)` crash-reconciliation row. `LifecycleEvent::MessageDropped` added (round-3
   DropOldest notice). Transition-table input cells that named non-`LogicalInput` guards (`Inbox`,
   `child terminal failure`, `last slot Done`, `EntityGate→Halted`) rewritten to name a real
   `LogicalInput` with the guard moved to the effect column.

APPLIED (Codex r4): slot↔cmd identity (`Pending { cmd }`) · `ActorCtx` on dispatch + result `ctx`
inheritance · agent UI write = sole `ToolReturned` (no agent `SurfaceMutated`) · mode folds Events
not Commands · native echo dedup by `cause` · payload-bearing `SurfaceOp` · `SurfaceObserved` + UI
fingerprints surface state · `SurfaceVersion` + conflict policy · mode-neutral System / no Driven
from absence · `edge_for`/`EdgeBound` receiver binding · de-conflated `SendPeer` duals + `PeerPayload`
· `SessionStarted`/abort-ack classification + Invariant 7 · `ChildReturned` dual exception · `Compact`
WAL key + reconciliation row · `MessageDropped` type · transition-table `LogicalInput` cells — all
addressed in place; Invariants 16–19 added; no other content altered.

---

## Appendix — Hardening lenses (apply these)

This schema is a baseline with KNOWN flaws found by an adversarial dry-run. The
fix is a principled pass: each lens below is applied to every type. Eleven
blocker-class flaws were identified; they map to the lenses as shown.

1. **Cardinality / multiplicity** (Minsky, dual). One-where-it-should-be-many or
   singleton-where-it-should-be-scoped. Blockers: B1 `UsingTool{cmd}` cannot hold
   N parallel tools nor batch N `ToolReturned` into one user message; parallel
   sub-agents have the same bug; singleton `ModelConfig`/`Autonomy`.
   **APPLIED:** `UsingTool{cmd}` → `UsingTools { pending: Map<ToolUseId,CmdId>,
   results: Vec<ToolResult> }` (transition onward only when `pending` empty; next
   turn emits ONE user Msg with all ToolResults in ToolUse order); added
   `AwaitingSubagents { children: Map<EntityId,ToolUseId>, results }` for parallel
   fan-out; `ModelConfig` & `Autonomy` made per-entity overridable
   (`Components.model`/`.autonomy`, else `Resources` default); `depth_cap` kept a
   single global scalar (depth is a per-path bound) with rationale stated;
   `WorldGate::Paused` now holds a set of concurrent `PauseHold`s. TurnSystem,
   ToolSystem, SubagentSystem and the Activity table updated to match. Noted (for
   the Totality pass) the unpaired duals `PeerDelivered`/`Timer` that lack a
   send/schedule Command.
2. **Separation of concerns / orthogonality** (Dijkstra 1974; Hunt & Thomas).
   Two independent concerns on one axis; policy/enforcement granularity mismatch.
   Blockers: B9 origin+edge not structural → mode-as-projection is uncomputable;
   `Clock=tick` conflates ordering vs wall-time; per-entity budget vs world halt.
   **APPLIED:** Added an `Event { origin: Origin, edge: EdgeId, at, input }` envelope
   (Stratum 1 logs Events) with `enum Origin { Human, Agent, System, Peer }`, and modeled
   `Edge`/`Counterpart` as first-class relationships (`Resources.edges`); restated mode as
   a pure per-edge fold `mode(edge) = classify(recent events by origin)`, making the
   concurrent Assisted+Driven case computable. Separated ordering (`clock: Tick`) from
   observed wall-time (new `Resources.wall: WallClock` datum; recording mechanism deferred
   to Hidden-Inputs). Split `ModelResponded` into `blocks` + `meta: ModelMeta { usage,
   model_id, stop_reason: StopReason }` (types only; transitions deferred to Totality).
   Matched gating granularity: kept App-wide `WorldGate` and added a distinct per-entity
   `Components.gate: EntityGate` so one entity's halt cannot freeze the World; documented
   two-level gating and kept Activity (progress) orthogonal to both gates. `Inbox` now
   holds `Event`s (replacing `Queued`). IntakeSystem/GateSystem and the Activity table
   updated to require BOTH gates Open. Left for later: `stop_reason` transitions (Totality),
   wall-time recording (Hidden-Inputs), budget-enforcement/recency-window logic (Boundedness).
3. **Totality / design-for-failure** (total transition functions; "everything
   fails"). Missing the denial/error/timeout/refusal edge; sink states. Blockers:
   B6 denied spawn/reject deadlock; UsingTool/AwaitingHuman are sinks &
   non-cancellable; B8 human-UI manipulation has no Input; B10 no cross-World
   driving interface; Anthropic `stop_reason: refusal | max_tokens | pause_turn`
   have no transition.
   **APPLIED:** Stated the TOTALITY INVARIANT ("no path leaves an entity waiting without
   an eventual result"); B6 — a denied `depth_cap` spawn, a rejected `Approval`, a tool
   error, and a budget/guardrail halt each synthesize a `ToolResult{is_error}` into the
   originating `ToolUse`. Gave every Activity state a defined exit: added `ModelFailed`
   (`Thinking → Idle` failure edge), `AwaitingHuman → (Thinking|Idle)` on
   `HumanResult ∈ {Provided,Declined,Timeout}`, and completed the partial-vs-complete
   `UsingTools`/`AwaitingSubagents` edges. Made cancellation per-state: generalized
   `Cancelling{awaiting: Vec<CmdId>}` with `UsingTools → Cancelling` (`CancelTool`),
   `AwaitingHuman → Cancelling` (`AbortHumanAction`), eager `AwaitingSubagents` abort, and
   `Cancel` defined (no-op + feedback) in idle/cancelling states. B8 — added first-class
   `SurfaceMutated{surface,element,op,value}` input (origin Human|Agent) + SurfaceSystem.
   B10 — added `DriveCommand`/`SendPeer`/`DriveRequested` (acked, authorized, recorded in
   the target's log) + sender-side `PeerSendOutcome` + PeerDriveSystem. Made the `Thinking`
   branch exhaustive over `StopReason` (`Refusal`→record+Idle; `MaxTokens`→continuation;
   `PauseTurn`→re-issue). Added the missing duals `SendPeer`↔`PeerDelivered`/`DriveRequested`
   and `ScheduleTimer`↔`Timer`. Completed `WorldGate::Paused{holds}` clearing (`Resume`
   clears `User` holds, `ClearPolicyHalt{trip,authority}` clears one PolicyHalt; reopen only
   when empty). Refreshed the Systems, the EXHAUSTIVE Activity transition table (with a
   stated default cell), and the WorldGate table. Left for later: WAL/idempotency for
   `SendPeer`/`CancelTool`/`ScheduleTimer` (lens 5); correlation-by-id of acks and `entity`
   on result inputs (lens 6); recording of `SurfaceMutated`/`DriveRequested`/wall-clock as
   hidden inputs (lens 4); continuation/retry caps (lens 7); de-dup of `SurfaceMutated` vs
   the originating `ToolUse`, and authority-token verification (lens 8 / enforcement).
4. **No hidden inputs** (referential transparency; functional core). A "pure"
   System reads ambient state. Blockers: B3 command-suppression-on-replay
   unstated; model-id/capability/reasoning-policy from a live table; wall-clock &
   RNG-seed unrecorded; HashMap entity iteration order; system-prompt date.
   **APPLIED:** Added a *Determinism discipline* section stating every System's only
   inputs are (World, Input). B3 — made replay/live SEPARATE DRIVERS (not a flag): Systems
   still emit Commands on replay; the replay driver DISCARDS them while the live driver
   dispatches, with logged Result Inputs standing in (stated as an invariant; record-and-
   replay principle updated). Defined the wall-time recording mechanism: an `Event.wall:
   Option<Timestamp>` stamp folded into `Resources.wall`, with the rule that Systems read
   wall-time ONLY from `WallClock` (the system-prompt date called out as a routed case).
   Recorded the RNG seed as a tick-0 `SessionStarted { seed }` header so a fresh replay
   reseeds identically; all randomness goes through `Rng`. Made routing/capability/reasoning
   per-call records: extended `ModelMeta` with `model_id` (effective), `capabilities`, and
   `reasoning: ReasoningPolicy {Echo|Drop|MustEcho}`, recorded so replay reproduces the same
   branch (single-source vs. a live table noted for lens 8). Mandated deterministic entity
   iteration: `entities: BTreeMap<EntityId,_>` with a defined order, banned exposed `HashMap`
   iteration in Systems, declared every `Map<…>` deterministically ordered, and flagged
   float avoidance for cross-arch replay. Updated the wall-time separation note and the
   Anthropic mapping. Left for later: WAL/idempotency (lens 5), id-correlation (lens 6),
   boundedness caps (lens 7), single-source dedup of per-call capability records (lens 8).
5. **Intent before commitment** (WAL; "exactly-once = at-least-once + idempotency").
   In-flight effects have no intent record → crash loses/duplicates. Blockers: B2
   in-flight effect on Pause/crash; effectful tools, UI mutations, and peer-send
   need WAL + idempotency keys + recovery reconciliation; log-before-apply.
   **APPLIED:** Added an *Intent before commitment* section stating
   **effectively-once = at-least-once + idempotency** (no true exactly-once). B2 — added a
   write-ahead **dispatch-intent** `CommandDispatched { cmd, kind, key, fingerprint }`
   (stratum 2, but LOAD-BEARING for recovery, unlike the neutral `WorkerCrashed`/`Tombstoned`/
   …), appended by the live driver BEFORE each effectful Command; on resume a `cmd` with a
   dispatch-intent but no result Input is an outstanding effect. Attached
   `key: IdempotencyKey = (app_id, tick, effect_id)` to `CallModel`/`RunTool`/
   `RequestHumanAction`/`SendPeer`/`ScheduleTimer` (provider-/tool-/broker-side dedup), leaving
   the cancel/abort duals key-free (inherently idempotent) and `RaiseInteraction` deduped by
   `request_id`. Classified tools (`ToolSet.ToolMeta.effect: Observational |
   Effectful{idempotent}`) so reconciliation can re-run vs. surface-as-failed. Defined the
   **replay→resume** asymmetry (replay SUPPRESSES Commands; resume RE-DISPATCHES outstanding
   ones) with a per-effect reconciliation table keyed on the tail Activity, including
   synthesizing + LOGGING a stratum-1 `InferenceCancelled { reason: Crash }` for a lost
   `CallModel` — amending "lifecycle events are behaviorally neutral": the neutral lifecycle
   record stays, but the crash's behavioral effect now lives in a stratum-1 Input
   (Autonomy/budget see it; replay reproduces it). Stated the **log-before-apply** invariant
   (no mutation before its causing Input is durably appended; World stays a function of the
   durable log). Covered UI-write WAL (agent `set_value` rides an Effectful `RunTool`, inherits
   dispatch-intent + key; client dedupes so no double-apply / world↔screen desync). Added
   `CommandDispatched`/`EffectKind`/`IdempotencyKey`/`EffectId`/`AppId`/`Fingerprint`/`ToolEffect`/
   `CancelReason`, a `Thinking → Idle` crash-reconciliation row, and a resume note in the
   Activity-table preamble and Residency note. Left for later: `effect_id` generation +
   fingerprint→result binding (lens 6), post-crash retry caps (lens 7), single canonical store
   reconciling the dispatch-intent log against any side store (lens 8).
6. **Correlate by identity, not position** (Correlation Identifier, EIP). Match by
   order instead of a stable key. Blockers: B4 positional replay → stale responses
   (fingerprint derived inputs; split exogenous vs derived); B11 cancel
   arbitration mis-keyed; deterministic core-generated ids; result Inputs need
   `entity`.
   **APPLIED:** Added a *Correlate by identity* section. B4 — made replay CONTENT-ADDRESSED:
   defined `Fingerprint` as a CANONICAL hash of the request payload (`CallModel`
   messages/tools/params, `RunTool` tool+args, …), recorded on the dispatch-intent AND echoed on
   every DERIVED result; on re-emit the replay driver compares the freshly-emitted request's
   fingerprint to the recorded one — match → reuse the cached result, mismatch → STALE → that
   branch falls to LIVE execution — which unifies fork and edit/rewind under one rule and stops a
   stale response from feeding an edited prefix. Split Stratum 1 into EXOGENOUS (free variables:
   `UserMessage`, `Pause`/`Resume`/`Cancel`/`ClearPolicyHalt`, `InteractionAnswer`, `SetAutonomy`,
   `PeerDelivered`, `DriveRequested`, human-origin `SurfaceMutated`) vs DERIVED (effect-result
   cache: `ModelResponded`/`ModelFailed`, `ToolReturned`, `InferenceCancelled`, `HumanActionDone`,
   `Timer`, `PeerSendOutcome`, agent-origin `SurfaceMutated`), marked as two groups in
   `LogicalInput`. B11 — re-keyed cancel arbitration on `(cmd, state)` (not a cmd-id mismatch,
   which still MATCHES in `Cancelling`): FIRST terminal input for a `cmd` finalizes, later
   duplicates are idempotently dropped, with the shell contract "exactly one terminal input per
   `cmd`" (synthesizes `InferenceCancelled` even if the call already completed). Made ids
   deterministic + core-generated: added `Resources.ids: IdAlloc` (monotonic `next_cmd`/
   `next_request`/`next_edge`/`next_timer`), defined `cmd`/`request_id`/`edge_id`/`TimerId`
   minting in the pure step and `effect_id` as the intra-tick ordinal — grounding Pass 5's
   `IdempotencyKey.effect_id`. Added `entity` (+ `fingerprint`) to `ModelResponded`/`ModelFailed`/
   `ToolReturned`/`InferenceCancelled`/`HumanActionDone`/`Timer`/`PeerSendOutcome` so results are
   self-routing and a late result for a cancelled `cmd` is matched-and-dropped, not misrouted;
   abort acks (`ToolAborted`/`HumanActionAborted`) carry `entity` but no fingerprint. Added
   `PeerEnvelopeId` to `PeerDelivered`/`DriveRequested` so cross-process redelivery de-dupes on
   the sender's stable envelope. Audited every correlation pair (`tool_use_id`↔`tool_result`,
   `cmd`+`fingerprint`↔result, `request_id`↔`InteractionAnswer`, child-entity↔parent `ToolUse`,
   `cmd`↔`PeerSendOutcome`, `envelope`↔inbound peer, `cmd`+`key`↔dispatch-intent) — all keyed,
   none positional. Updated `Fingerprint`/`EffectId` comments, the record-and-replay principle,
   the *Determinism discipline* replay-driver invariant, the *Intent before commitment* deferral
   note (effect_id + fingerprint binding now supplied here), and CancelSystem. Left for later:
   boundedness caps (lens 7); single-canonical-store reconciliation, incl. de-dup of the
   agent-origin `SurfaceMutated` echo vs its `ToolUse` (lens 8).
7. **Boundedness / backpressure** (Reactive Streams). A loop/queue/growth with no
   brake. Items: tool-loop cap; context compaction; restore snapshots (avoid
   O(history)); process-budget admission/eviction; unbounded `Inbox`,
   `RaisedInteractions`, peer `inbox.jsonl`.
   **APPLIED:** Added a *Boundedness and backpressure* section putting a brake on every loop,
   queue, growing structure, and retry. LOOP GUARD: `Components.limits.turns` + `Resources.loop_cap`
   bound the tool/turn loop; on exhaustion the guard withholds tools and injects an `is_error`
   `ToolResult` (Pass 3's recovery rule) to force a final wrap-up turn (IntakeSystem resets,
   ToolSystem/TurnSystem increment; `MaxTokens`/`PauseTurn` continuations count). FAN-OUT: added the
   per-subtree `Components.limits.spawned` + `Resources.fanout_cap` (the count budget the Cardinality
   pass deferred), distinct from `depth_cap`; SubagentSystem denies past either with an `is_error`
   slot. TWO BUDGETS, explicitly distinguished: SPEND (`TokenBudget.used`/`limit`, cumulative → HALT)
   vs CONTEXT window (`context_used`/`context_limit`, single-request → COMPACT). Wired the deferred
   **BudgetSystem**: folds `meta.usage` after `ModelResponded`, sets per-entity
   `EntityGate::Halted{BudgetExhausted}` (NOT the world gate) + `is_error` on spend exhaustion, and
   triggers compaction on context pressure. Added **CompactionSystem** + `Compact` Command +
   `Compacted` Input + a `Compacting` Activity state (best-effort on compaction `ModelFailed`; no new
   sink). SNAPSHOTS: tick-indexed World snapshots every `Caps.snapshot_interval`; restore = nearest
   snapshot ≤ tick + tail replay, fixing O(history)→O(n²) and bounding crash-reconciliation + memory
   (snapshots are a DERIVED cache; log stays single source — flagged for lens 8). PROCESS BUDGET:
   soft cap `N` with an admission rule (admit `N+1` then converge OR queue), idle-only LRU eviction
   (never evict a mid-turn app — that fakes a crash), and a starvation backstop (escalate when all
   slots are busy/`AwaitingHuman`). QUEUE CAPS via `Resources.caps`: `Inbox` DropOldest (Paused app
   can't grow it), `RaisedInteractions` RejectNewest (→ `is_error`), peer inbox RejectNewest (→
   `Rejected` → `is_error` to sender). RETRY: `Caps.restart` adds exponential backoff + a
   restart-intensity give-up/escalation bound (also governs `ModelFailed`). LARGE BLOBS: `Image`/
   large `ToolResult` externalized to a content-addressed blob store referenced by hash (size only;
   dedup/consistency NOTED for lens 8). Bounded the mode-as-projection window (`Caps.mode_window`).
   Updated Components/Resources/TokenBudget/Activity/Command/`EffectKind`/`LogicalInput`, the Systems
   list, the Activity table, the Residency note, the correlation-pair audit, and the Intent-before-
   commitment deferral. Left for later: single-source/dedup of snapshots-vs-log, blob-store dedup,
   and the `RaisedInteractions`/capability-table reconciliation (lens 8); authority/quota enforcement
   policy values (enforcement).
8. **Single source of truth** (event sourcing; Fowler). The same fact recorded
   twice → drift. Items: History vs the logical log; `resume.json` out-of-log;
   capability metadata recorded per-call vs a table; one canonical store.
   **APPLIED:** Added a *Single source of truth* section declaring the per-App
   **logical Input log (+ snapshots as a cache) the SINGLE authority**, with
   `History`/`raised`/`TokenBudget`/`WorldGate`/`EntityGate`/`Activity`/`Inbox`/`edges`/
   `IdAlloc`/`mode` all PROJECTIONS rebuilt by replaying the log through the Systems
   (the log wins on conflict). Declared `History` a materialized projection (cache for
   the next `CallModel`, reconstructable on replay) and reconciled `session.jsonl` as a
   DERIVED VIEW (log canonical; the export is regenerable, never read back as authority).
   ELIMINATED `resume.json`: pending interactions (`raised`), pending human asks
   (`AwaitingHuman` + its dispatch-intent), the `Inbox`, and undelivered peer messages
   all rebuild from the log (issuance of `RaiseInteraction`/`RequestHumanAction` re-emits
   on replay; the broker peer-queue is reconstructable from `PeerSendOutcome{Queued}` +
   `PeerDelivered`/`DriveRequested`), shrinking the side file to nothing — any residual
   cache requires atomic temp+fsync+rename and is cache, not truth. Made SNAPSHOTS a
   derived cache (`snapshot(t)=replay(log[..t])`): validated against tick, discarded and
   regenerated on disagreement, with a **pruning rule** (keep the latest K; older are
   regenerable; the log is never pruned below regeneration need). Declared the per-call
   `ModelMeta` (`model_id`/`capabilities`/`reasoning`) AUTHORITATIVE on replay and the
   live table LIVE-ONLY (consulted only to produce the record; the record supersedes it).
   Stated content-addressed blobs are single-source BY CONSTRUCTION (one entry per hash;
   the log references by hash; dedup by hash). De-duped the agent UI-write echo: the
   `set_value` `RunTool` (`ToolUse` + `CommandDispatched` + `ToolReturned`) is CANONICAL,
   and `SurfaceMutated{origin:Agent}` is a projection of that effect folded exactly once
   keyed by `cmd` (human-origin `SurfaceMutated` stays a primary exogenous Input). Added a
   dual-source audit table and an *Invariants* checklist consolidating all eight passes.
   Updated the deferral notes in *Boundedness*, *Intent before commitment*, the `ModelMeta`/
   `Image`/`SurfaceMutated` comments, and *Determinism discipline* to point here. Nothing
   deferred — this is the final pass.

Sources: Minsky "make illegal states unrepresentable"; Dijkstra, *On the Role of
Scientific Thought* (1974); Hunt & Thomas, *The Pragmatic Programmer*; Bernhardt,
*Functional Core, Imperative Shell*; Hohpe & Woolf, *Enterprise Integration
Patterns* (Correlation Identifier); database WAL; Fowler, *Event Sourcing*.
