//! world — see docs/agent/world/ecs-runtime.md

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::autonomy::Autonomy;
use super::budget::Budget;
use super::effects::ToolSet;
use super::gates::{EntityGate, WorldGate};
use super::history::{Block, History, ToolResult};
use super::surface::{PeerEnvelopeId, SurfaceView};

/// Default value for `Resources.autonomy` when absent from a serialised log
/// (old sessions pre-date the field). `RunFree` is the conservative default:
/// an existing session that was already running without approval gates should
/// not suddenly start asking for approval on resume.
fn default_autonomy() -> Autonomy {
    Autonomy::RunFree
}

/// Default value for `Resources.depth_cap` when absent from a serialised log (old
/// sessions pre-date the field). Bounds sub-agent NESTING along any root→leaf path
/// (checked against per-entity `Lineage.depth`). See docs/agent/world/ecs-runtime.md.
fn default_depth_cap() -> u8 {
    8
}

// ---------------------------------------------------------------------------
// Core frame — id and time aliases
// ---------------------------------------------------------------------------

/// Logical time: a monotonic ORDERING counter. NOT wall-time — observed
/// wall-time lives in `Resources.wall` as a separate datum (Invariant 2).
pub type Tick = u64;
/// An agent within this World (primary or an in-process sub-agent).
pub type EntityId = u32;
/// A core-minted command id, correlating a dispatched effect to its result.
pub type CmdId = u32;
/// A core-minted interaction-request id.
pub type ReqId = u32;
/// A core-minted counterpart-relationship id.
pub type EdgeId = u32;
/// A core-minted timer id.
pub type TimerId = u32;
/// Observed wall-time as a datum (shape only; encoding fixed by a later pass).
pub type Timestamp = i64;

// ---------------------------------------------------------------------------
// World — the single serializable value rebuilt by replaying the log
// ---------------------------------------------------------------------------

/// The whole runtime state for one App. `entities` is a `BTreeMap` (never a
/// `HashMap`): deterministic iteration order is load-bearing for replay, since
/// a `HashMap`'s per-process random order would be a hidden input that diverges
/// `World'` (Invariant 8). See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct World {
    /// Logical ORDERING counter only (Invariant 2).
    pub clock: Tick,
    /// The App's primary agent.
    pub root: EntityId,
    /// Primary + in-process sub-agents, ORDERED by `EntityId`.
    pub entities: BTreeMap<EntityId, Components>,
    /// World-level singletons.
    pub resources: Resources,
}

impl World {
    /// A fresh World at tick 0 with no entities, owning the given `resources`.
    pub fn new(root: EntityId, resources: Resources) -> Self {
        World {
            clock: 0,
            root,
            entities: BTreeMap::new(),
            resources,
        }
    }
}

// ---------------------------------------------------------------------------
// Components — per entity (= per agent). P0 subset.
// ---------------------------------------------------------------------------

/// The P0 per-entity state: who it is, where it sits in the lineage, its
/// conversation, what it is currently doing, and an optional per-entity model
/// override (`None` ⇒ inherit `Resources.model`). Later phases widen this with
/// budget/limits/gate/inbox/tools (see the design doc); they are out of P0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Components {
    pub identity: Identity,
    pub lineage: Lineage,
    pub history: History,
    pub activity: Activity,
    /// Per-entity run-gate ("am I allowed to run"), orthogonal to `activity`
    /// (turn progress) and to the world-level `Resources.gate`. A closed gate
    /// blocks this entity's turn INITIATION/CONTINUATION, never result-settling
    /// (Invariant 13). See docs/agent/world/ecs-runtime.md (EntityGate).
    pub gate: EntityGate,
    /// Per-entity token budget: the cumulative `spend` account (→ per-entity HALT
    /// on exhaustion, owned by BudgetSystem) and the single-request `context`
    /// account (→ compaction), kept DISTINCT. Folded from each
    /// `ModelResponded.meta.usage`. See docs/agent/world/ecs-runtime.md
    /// (BudgetSystem; Two budgets: spend vs context).
    pub budget: Budget,
    /// Per-entity FIFO of inbound user messages STEERED while the entity was
    /// mid-run: a `UserMessage` arriving in an active state is parked here by
    /// SteeringSystem (it never interrupts the in-flight turn) and DRAINED by
    /// IntakeSystem into the next turn it initiates from `Idle`. See
    /// docs/agent/world/ecs-runtime.md (Inbox; SteeringSystem; IntakeSystem).
    pub inbox: Inbox,
    /// Per-entity LOOP-GUARD counter: the number of turns IntakeSystem has INITIATED
    /// for this entity (Boundedness — Inv 11). It increments on each initiation; on
    /// reaching `budget.limits.loop_cap` (cap != 0) IntakeSystem forces ONE final
    /// wrap-up turn and then stops initiating. The brake is World state, so replay
    /// reproduces it exactly. `0` for a fresh entity. `#[serde(default)]` so logs
    /// written before this field deserialise. See docs/agent/world/ecs-runtime.md
    /// (Loop guard — bound the tool/turn loop).
    #[serde(default)]
    pub turns: u32,
    /// Per-entity CUMULATIVE fan-out counter: the number of child sub-agents this
    /// entity has EVER spawned across its whole lifetime (Boundedness — Inv 11).
    /// Bumped by SubagentSystem on each successful spawn; NEVER decremented when a
    /// child completes or is removed (cumulative, not live-count). Bounded by
    /// `budget.limits.fanout_cap`; `0` for a fresh entity. `#[serde(default)]` so
    /// M1/M2 logs and snapshots deserialise byte-identically. See
    /// docs/agent/world/ecs-runtime.md (Fan-out budget — bound sub-agent spawning).
    #[serde(default)]
    pub spawned: u32,
    /// Per-entity override of the world-default `ModelConfig` (`None` ⇒ inherit).
    pub model: Option<ModelConfig>,
}

/// A per-entity FIFO of inbound user messages parked while the entity is mid-run
/// (turn STEERING). SteeringSystem pushes a mid-run `UserMessage`'s content onto
/// `pending` rather than folding it into the in-flight turn; IntakeSystem DRAINS
/// `pending` in arrival (FIFO) order into the next turn it initiates from `Idle`,
/// honouring the queued messages as user input. Each entry is one message's
/// content blocks. The queue is BOUNDED by `Budget.limits.inbox_capacity` with
/// DropOldest backpressure: SteeringSystem evicts the oldest entry past the cap and
/// emits a stratum-2 `MessageDropped` (`0` ⇒ unbounded). See
/// docs/agent/world/ecs-runtime.md (Inbox; Bounded queues — Inbox DropOldest).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    /// Queued user-message contents, OLDEST FIRST (FIFO).
    pub pending: Vec<Vec<Block>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Identity {
    Primary,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    pub parent: Option<EntityId>,
    pub depth: u8,
}

/// A tool-use block id; matches `ToolUse.id` and `ToolResult.tool_use_id`
/// in the Anthropic Messages schema. See docs/agent/world/ecs-runtime.md.
pub type ToolUseId = String;

/// The per-entity turn state machine. Extends the P0 `Idle`/`Thinking` pair
/// with the P1a tool-resolution, compaction, and cancellation states. Vec-bearing
/// variants make this non-`Copy`; `ToolResult` (= `Block`) contains
/// `serde_json::Value` which is not `Eq`, so `Eq` is likewise absent.
/// See docs/agent/world/ecs-runtime.md (Activity enum).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Activity {
    Idle,
    /// Awaiting `ModelResponded` | `ModelFailed`.
    Thinking { cmd: CmdId },
    /// One assistant turn emitted mixed tool_use blocks; each becomes a `ToolSlot`
    /// resolved independently. The entity advances only once EVERY slot is `Done`.
    /// The next turn assembles exactly one user Msg with results in ascending
    /// `ordinal` order (Anthropic requires all tool_results in one following user
    /// message). See docs/agent/world/ecs-runtime.md (ResolvingToolUses).
    ResolvingToolUses { slots: Vec<ToolSlot> },
    /// A context-window compaction call is in flight (Boundedness). On `Compacted`
    /// History is rewritten and the entity transitions to `Thinking`; on
    /// `ModelFailed` it proceeds un-compacted. See docs/agent/world/ecs-runtime.md.
    Compacting { cmd: CmdId },
    /// Abort acks are still owed for the listed cmds. Each ack (or a racing real
    /// result) clears its entry; when `awaiting` is empty the entity goes `Idle`.
    /// See docs/agent/world/ecs-runtime.md (Cancelling).
    Cancelling { awaiting: Vec<CmdId> },
}

// ---------------------------------------------------------------------------
// Tool-slot model — per-assistant-turn mixed resolution (P1a)
// ---------------------------------------------------------------------------

/// One outstanding tool_use block from a single assistant turn. `ordinal` is the
/// block's position in the assistant message — the `tool_result` ordering key —
/// NOT the arrival order of results. Each slot resolves independently via its
/// kind's terminal Input; see docs/agent/world/ecs-runtime.md (ToolSlot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSlot {
    /// The Anthropic `tool_use_id` / `tool_use.id` this slot backs.
    pub tool_use_id: ToolUseId,
    /// Position of this block in the assistant message — the ordering key for the
    /// assembled `tool_result` user Msg (ascending ordinal, NOT arrival order).
    pub ordinal: u16,
    /// What kind of executor resolves this slot.
    pub kind: SlotKind,
    /// Current resolution state.
    pub state: SlotState,
    /// Filled once `state == Done`. An `is_error` `ToolResult` on any
    /// denial/error/timeout/decline/halt (Totality Invariant).
    pub result: Option<ToolResult>,
}

/// What kind of executor resolves a `ToolSlot`. `Child` carries the spawned
/// child entity so `ChildReturned` can be correlated by `(child, tool_use_id)`.
/// See docs/agent/world/ecs-runtime.md (SlotKind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    /// A local tool call; resolved by `ToolReturned { cmd }`.
    Local,
    /// A child sub-agent; resolved by `ChildReturned { child, tool_use_id }`.
    Child(EntityId),
    /// A human action; resolved by `HumanActionDone { cmd }`.
    Human,
    /// A peer drive; resolved by `PeerSendOutcome { cmd }`.
    Peer,
}

/// Resolution state of a `ToolSlot`. A `Pending` slot carries the IDENTITY by
/// which its terminal result is matched back to it (Invariant 16):
/// `Local`/`Human`/`Peer` slots hold `Some(cmd)` (matched by the outstanding
/// shell-dispatched command id); a `Child` slot holds `None` (correlated by
/// `(child, tool_use_id)` via `SlotKind::Child(EntityId)` instead).
/// See docs/agent/world/ecs-runtime.md (SlotState).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotState {
    /// Awaiting its terminal Input. `cmd` identifies the outstanding command for
    /// `Local`/`Human`/`Peer` slots; `None` for `Child` slots (identity-correlated).
    Pending { cmd: Option<CmdId> },
    /// Terminal Input received; `ToolSlot.result` is `Some`.
    Done,
}

/// Sentinel `CmdId` stored in a gated slot's `Pending { cmd: Some(HELD_CMD) }` to
/// prevent ToolSystem from dispatching the slot while its approval interaction is
/// pending. `u32::MAX` is reserved — `IdAlloc::mint_cmd` saturates below it so no
/// real dispatched cmd can ever collide with this value. AutonomySystem sets this
/// sentinel on gate; it clears it to `Pending { cmd: None }` on `InteractionAnswer`
/// so InteractionSystem can release the slot. CancelSystem treats any slot in this
/// state as a held-for-approval slot: it resolves the slot immediately (no abort,
/// no `awaiting` entry) rather than waiting forever on an ack that never arrives
/// (Invariant 10/14 — no infinite await). See docs/agent/world/ecs-runtime.md
/// (AutonomySystem; CancelSystem; HELD_CMD sentinel soundness).
pub const HELD_CMD: CmdId = u32::MAX;

// ---------------------------------------------------------------------------
// Caps — bounded-resource knobs (World-state, replayable). P0 subset.
// ---------------------------------------------------------------------------

/// The capacity of a bounded queue. `0` means **unbounded** — the brake is
/// intentionally disabled. Used for every queue-depth cap in [`Caps`].
///
/// See docs/agent/world/ecs-runtime.md §"Boundedness"/Caps.
pub type QueueCap = u32;

/// Bounded-resource knobs for the World: every queue, growing structure, and
/// retry loop carries an explicit brake here so the runtime cannot grow without
/// bound. `Caps` is World state (a `Resources` singleton) and therefore replays
/// deterministically with everything else. A value of `0` in any numeric cap
/// means **unbounded** — the brake is intentionally disabled.
///
/// See docs/agent/world/ecs-runtime.md §"Boundedness"/Caps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    /// Ticks between consecutive World snapshots. A snapshot anchors restore
    /// to `nearest-snapshot + tail` (Inv 9). `0` = no periodic snapshots.
    pub snapshot_interval: Tick,
    /// Maximum number of snapshots to retain per branch. Older snapshots
    /// beyond this count are eligible for eviction (Inv 11). `0` = unbounded.
    pub snapshot_keep: u32,
    /// Grace window (in ticks) after a branch goes dead before it is eligible
    /// for reclaim. `0` = unbounded (dead branches never auto-expire).
    pub branch_grace: Tick,
    /// Maximum number of peer messages that may sit in the durable peer inbox
    /// of any one App. When the inbox is at this depth, a new send is rejected
    /// (RejectNewest): nothing is enqueued and the sender receives
    /// `DeliveryOutcome::Rejected`. `0` = unbounded (no backpressure). See
    /// docs/agent/world/ecs-runtime.md §"Durable peer delivery" (§1335–1338).
    pub peer_inbox: QueueCap,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            snapshot_interval: 64,
            snapshot_keep: 3,
            // 0 = unbounded: dead branches never auto-expire by default;
            // branch-reclaim logic (a later milestone) reads this and skips
            // eviction when the value is 0.
            branch_grace: 0,
            // 0 = unbounded: no inbox backpressure by default; an operator
            // sets a positive value to bound the per-App peer inbox.
            peer_inbox: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Resources — world singletons. P0 subset.
// ---------------------------------------------------------------------------

/// World-level singletons. P0 carries the determinism spine (`rng`, `wall`,
/// `ids`), the world-default model config, the (empty for P0) edge table, the
/// App-wide world gate, and the per-surface view. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    /// The SOLE source of randomness in the World; seeded from `SessionStarted`.
    pub rng: Rng,
    /// Last OBSERVED wall-time (a datum), NOT the ordering axis.
    pub wall: WallClock,
    /// Anthropic params — the WORLD DEFAULT; `Components.model` overrides it.
    pub model: ModelConfig,
    /// Counterpart relationships, keyed by `EdgeId`. Empty in P0.
    pub edges: BTreeMap<EdgeId, Edge>,
    /// App-wide run-state gate (User pause / PolicyHalt), owned by GateSystem.
    /// See `WorldGate` in `gates`.
    pub gate: WorldGate,
    /// The SOLE id minter; mints ids as pure functions of World state.
    pub ids: IdAlloc,
    /// Per-surface view: latest observed state of each surface, keyed by
    /// `SurfaceId`. Uses a deterministic map (Invariant 8 — a random-order
    /// map's iteration is a hidden input that diverges `World'` on replay).
    /// Empty until `SurfaceSystem` (task U-surface-system) folds the first
    /// `SurfaceObserved` input. See docs/agent/world/ecs-runtime.md.
    pub surfaces: SurfaceView,
    /// Pending `RaiseInteraction` requests, keyed by `request_id`. Populated
    /// by AutonomySystem (or any System emitting `RaiseInteraction`) so the
    /// InteractionSystem can correlate a later `InteractionAnswer` back to the
    /// entity and `tool_use_id` whose action is held. Cleared on answer.
    /// Uses a deterministic `BTreeMap` (Inv 8). See docs/agent/world/ecs-runtime.md
    /// (InteractionSystem).
    #[serde(default)]
    pub raised: BTreeMap<ReqId, (EntityId, ToolUseId)>,
    /// World-default autonomy policy; per-entity overrides live in
    /// `Components.autonomy` (a future milestone). Resolution is entity override
    /// else this default. Updated by `SetAutonomy`; owned by AutonomySystem.
    /// See docs/agent/world/ecs-runtime.md (Autonomy/Tier; AutonomySystem).
    #[serde(default = "default_autonomy")]
    pub autonomy: Autonomy,
    /// Max sub-agent NESTING depth along any root→leaf path — a single global
    /// scalar checked against a prospective child's `Lineage.depth`. SubagentSystem
    /// DENIES a spawn that would exceed it with an `is_error` result (Inv 10), never
    /// leaving the parent waiting. The per-subtree fan-out count cap (`fanout_cap`)
    /// is a separate Boundedness concern, deferred. See docs/agent/world/ecs-runtime.md.
    #[serde(default = "default_depth_cap")]
    pub depth_cap: u8,
    /// The surface-manipulation tools the App's surface-capable ROOT entity is
    /// offered on its model calls — the source the CallModel-emitting Systems read
    /// into a turn's `ToolSet` so a live model call is told the tools EXIST and can
    /// request them (e.g. `set_value`). Empty by default ⇒ a tool-less call whose
    /// request body is byte-identical to a pre-tools call (replay/fingerprint
    /// stability); the live App's tick-driver populates it for the whiteboard
    /// surface entity. A non-root entity (a sub-agent) is offered none. `#[serde(
    /// default)]` so logs written before this field deserialise. See
    /// docs/agent/world/ecs-runtime.md (Anthropic model-call mapping; SurfaceSystem).
    #[serde(default)]
    pub surface_tools: ToolSet,
    /// Bounded-resource knobs: snapshot cadence, retention limit, and branch
    /// grace window. `#[serde(default)]` so Milestone-1 logs (which carry no
    /// `caps` key) deserialise byte-identically with the struct defaults.
    /// See docs/agent/world/ecs-runtime.md §"Boundedness"/Caps.
    #[serde(default)]
    pub caps: Caps,
    /// The inbound peer envelope ids this World has ALREADY durably folded — the
    /// receiver's STRATUM-1 dedup authority that makes peer delivery
    /// effectively-once. PeerDriveSystem records each `DriveRequested`/
    /// `PeerDelivered` envelope here AS PART OF folding it (the record is created
    /// BY the fold, atomic with the World-log append), and treats a redelivery
    /// whose envelope is already present as a NO-OP trace — never re-folded. The
    /// authority lives HERE, not in a broker-side ledger written before the
    /// receiver applies: because the dedup record is created only by the fold, a
    /// crash BEFORE the fold leaves NO dedup, so the redelivery re-folds
    /// (at-least-once transport + this idempotency = effectively-once, with no
    /// crash window that both dedups and drops a message). A replay reconstructs
    /// the set by re-folding the same EXOGENOUS peer inputs, so it is not recorded
    /// separately. A deterministic `BTreeSet` (never a `HashMap`) keeps membership
    /// free of a hidden ordering input (Inv 8). `#[serde(default)]` so M1/M2
    /// logs/snapshots (which carry no peer envelopes) deserialise byte-identically.
    /// See docs/agent/world/ecs-runtime.md §"Durable peer delivery".
    #[serde(default)]
    pub applied_envelopes: BTreeSet<PeerEnvelopeId>,
}

impl Resources {
    /// World singletons seeded for a fresh session: `rng` from `seed`, an
    /// unobserved `wall`, the given world-default `model`, no edges, an open
    /// gate, a zeroed allocator, an empty surface view, no raised interactions,
    /// a `RunFree` autonomy policy (approve everything by default), and
    /// default `Caps` (snapshot_interval=64, snapshot_keep=3, branch_grace=0).
    pub fn new(seed: u64, model: ModelConfig) -> Self {
        Resources {
            rng: Rng::seeded(seed),
            wall: WallClock::default(),
            model,
            edges: BTreeMap::new(),
            gate: WorldGate::default(),
            ids: IdAlloc::default(),
            surfaces: BTreeMap::new(),
            raised: BTreeMap::new(),
            autonomy: Autonomy::RunFree,
            depth_cap: default_depth_cap(),
            // No surface tools by default ⇒ a tool-less call (byte-identical to a
            // pre-tools request). The live tick-driver opts the surface entity in.
            surface_tools: ToolSet::default(),
            caps: Caps::default(),
            // No peer envelope folded yet: the receiver's stratum-1 dedup set is
            // empty at genesis and grows only as PeerDriveSystem folds inbound
            // peer inputs.
            applied_envelopes: BTreeSet::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// IdAlloc — the sole id minter; pure functions of World state
// ---------------------------------------------------------------------------

/// Monotonic id allocator — the SOLE minter of new ids. Each `mint_*` is a PURE
/// function: it returns the minted id together with the advanced allocator, so
/// a replay mints byte-identical ids (Invariant 8). Ids are NEVER shell-assigned.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdAlloc {
    pub next_cmd: u32,
    pub next_request: u32,
    pub next_edge: u32,
    pub next_timer: u32,
    /// Next in-process sub-agent `EntityId`. `#[serde(default)]` so logs written
    /// before in-process sub-agents still deserialise. The root primary agent is
    /// entity `0`, so `mint_entity` yields ids starting at `1` (see there).
    #[serde(default)]
    pub next_entity: u32,
}

impl IdAlloc {
    /// Mint the next `CmdId`, returning it with the advanced allocator.
    ///
    /// `HELD_CMD` (`u32::MAX`) is reserved as the gated-slot sentinel and is NEVER
    /// returned by this function. The allocator saturates at `u32::MAX - 1`: once
    /// `next_cmd` would advance into `u32::MAX`, it stays at `u32::MAX - 1` instead,
    /// so no real dispatched `CmdId` can ever equal `HELD_CMD`. Minting
    /// 4 294 967 294 cmds in a single session is not reachable in practice;
    /// `debug_assert` catches the overflow boundary in tests.
    pub fn mint_cmd(self) -> (CmdId, IdAlloc) {
        let id = self.next_cmd;
        debug_assert!(id < u32::MAX, "mint_cmd: next_cmd reached HELD_CMD sentinel (u32::MAX)");
        // Advance next_cmd but never into u32::MAX (HELD_CMD). saturating_add caps at
        // u32::MAX; .min(u32::MAX - 1) then keeps it one below the sentinel.
        let next_cmd = id.saturating_add(1).min(u32::MAX - 1);
        (id, IdAlloc { next_cmd, ..self })
    }

    /// Mint the next `ReqId`, returning it with the advanced allocator.
    pub fn mint_request(self) -> (ReqId, IdAlloc) {
        let id = self.next_request;
        (
            id,
            IdAlloc {
                next_request: id + 1,
                ..self
            },
        )
    }

    /// Mint the next `EdgeId`, returning it with the advanced allocator.
    pub fn mint_edge(self) -> (EdgeId, IdAlloc) {
        let id = self.next_edge;
        (
            id,
            IdAlloc {
                next_edge: id + 1,
                ..self
            },
        )
    }

    /// Mint the next `TimerId`, returning it with the advanced allocator.
    pub fn mint_timer(self) -> (TimerId, IdAlloc) {
        let id = self.next_timer;
        (
            id,
            IdAlloc {
                next_timer: id + 1,
                ..self
            },
        )
    }

    /// Mint the next in-process sub-agent `EntityId`, returning it with the advanced
    /// allocator. Entity `0` is the root primary agent, so minted child ids start at
    /// `1` — a freshly minted sub-agent never collides with the root. Pure, like
    /// every `mint_*`, so a replay mints byte-identical child ids (Invariant 8).
    pub fn mint_entity(self) -> (EntityId, IdAlloc) {
        let id = self.next_entity.max(1);
        (
            id,
            IdAlloc {
                next_entity: id.saturating_add(1),
                ..self
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Rng — the sole, deterministic source of randomness
// ---------------------------------------------------------------------------

/// Deterministic PRNG — the SOLE source of randomness in the World. `seed`
/// enters the log once as `SessionStarted { seed }`, so a fresh replay reseeds
/// identically and every draw reproduces (Invariant 8). Systems draw only
/// through this Resource, never an ambient RNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rng {
    pub seed: u64,
    pub state: u64,
}

impl Rng {
    /// Seed the generator; `state` starts at `seed`.
    pub fn seeded(seed: u64) -> Self {
        Rng { seed, state: seed }
    }

    /// Draw the next value, returning it with the advanced generator (pure).
    /// SplitMix64 — no ambient entropy, so replay reproduces every draw.
    pub fn next_u64(self) -> (u64, Rng) {
        let state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (
            z,
            Rng {
                seed: self.seed,
                state,
            },
        )
    }
}

// ---------------------------------------------------------------------------
// WallClock — observed wall-time, separated from the ordering axis
// ---------------------------------------------------------------------------

/// The last wall-time the shell observed, fed from `Event.wall`. The agent reads
/// it as data (e.g. "what is the date today"); Systems read wall-time ONLY here,
/// NEVER `now()` (Invariant 2). `None` until first observed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WallClock {
    pub observed: Option<Timestamp>,
}

// ---------------------------------------------------------------------------
// Edges — first-class counterpart relationships (mode folds per edge)
// ---------------------------------------------------------------------------

/// World-side peer address. Mirrors `app::peer::PeerId` (which is
/// `{ app_id: AppId(String), node_id: NodeId(String) }`) so a shell `PeerId`
/// serialises into the World log byte-identically — the two newtypes `AppId`
/// and `NodeId` are transparent to serde, leaving a `{"app_id":"…","node_id":"…"}`
/// wire shape that this struct replicates with plain `String` fields. Defined
/// separately to keep the World functional core free of the shell layer.
/// See docs/agent/world/ecs-runtime.md §301–309.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct PeerId {
    pub app_id: String,
    pub node_id: String,
}

/// A first-class relationship between this World and a counterpart. Mode is a
/// pure fold over the origins of recent Events on a given edge (see the
/// Mode-as-projection pass). P0 only models the human edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub counterpart: Counterpart,
}

/// The counterpart on the far side of an `Edge`.
///
/// - `Human` — the human-facing surface (the native client UI).
/// - `App`   — this App's own surface (the macOS accessibility layer the agent
///             writes to via `RunTool`/`set_value`).
/// - `Peer`  — a peer App process on the network, introduced by the federation
///             milestone. `edge_for(Peer(id))` binds one stable edge per peer
///             so inbound `DriveRequested`/`PeerDelivered` inputs are always
///             recorded on a replayable, identity-keyed edge.
///
/// See docs/agent/world/ecs-runtime.md §298–309.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Counterpart {
    Human,
    App,
    Peer(PeerId),
}

// ---------------------------------------------------------------------------
// ModelConfig — Anthropic params
// ---------------------------------------------------------------------------

/// Anthropic call parameters. World default in `Resources.model`; an optional
/// per-entity override lives in `Components.model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model: String,
    pub max_tokens: u32,
    pub effort: Effort,
}

/// Reasoning/output effort, mapped to `output_config.effort` in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::history::{Block, History, Msg, Role, ToolResult};

    fn sample_model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    #[test]
    fn world_round_trips() {
        let mut world = World::new(0, Resources::new(42, sample_model()));
        world.clock = 7;
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage {
                    parent: None,
                    depth: 0,
                },
                history: History(vec![Msg {
                    role: Role::Assistant,
                    content: vec![Block::Text {
                        text: "hello".into(),
                    }],
                }]),
                activity: Activity::Thinking { cmd: 3 },
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 4,
                spawned: 2,
                model: None,
            },
        );
        world.entities.insert(
            1,
            Components {
                identity: Identity::Subagent,
                lineage: Lineage {
                    parent: Some(0),
                    depth: 1,
                },
                history: History::default(),
                activity: Activity::Idle,
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 0,
                spawned: 0,
                model: Some(sample_model()),
            },
        );

        let json = serde_json::to_string(&world).expect("serialise world");
        let back: World = serde_json::from_str(&json).expect("deserialise world");
        assert_eq!(world, back);
    }

    #[test]
    fn id_mint_is_a_pure_function_of_idalloc() {
        let ids = IdAlloc::default();
        let (c0, ids) = ids.mint_cmd();
        let (c1, _ids) = ids.mint_cmd();
        assert_eq!(c0, 0);
        assert_eq!(c1, 1);

        // Purity: minting from a fixed IdAlloc always yields the same id and the
        // same advanced allocator — the basis of byte-identical replay ids.
        let (again, next) = IdAlloc::default().mint_cmd();
        assert_eq!(again, c0);
        assert_eq!(next, IdAlloc::default().mint_cmd().1);

        // Counters are independent.
        let (r0, after_req) = IdAlloc::default().mint_request();
        assert_eq!(r0, 0);
        assert_eq!(after_req.next_cmd, 0);
    }

    #[test]
    fn rng_is_deterministic_for_a_fixed_seed() {
        let (a, _) = Rng::seeded(99).next_u64();
        let (b, _) = Rng::seeded(99).next_u64();
        assert_eq!(a, b);
    }

    // -----------------------------------------------------------------------
    // P1a slot tests
    // -----------------------------------------------------------------------

    /// Build a mixed-kind `ResolvingToolUses` with four slot kinds, verify that
    /// ordinals are preserved through a serde round-trip, and that the identity
    /// invariant (Inv 16) holds: Child → `cmd: None`, Local → `Some(cmd)`.
    #[test]
    fn resolving_tool_uses_round_trips_with_mixed_slot_kinds() {
        let tool_result: ToolResult = Block::ToolResult {
            tool_use_id: "tu_local".into(),
            content: vec![Block::Text {
                text: "ok".into(),
            }],
            is_error: false,
        };

        let slots = vec![
            // ordinal 0 — Local slot: carries a cmd (Inv 16)
            ToolSlot {
                tool_use_id: "tu_local".into(),
                ordinal: 0,
                kind: SlotKind::Local,
                state: SlotState::Pending { cmd: Some(7) },
                result: None,
            },
            // ordinal 1 — Child slot: no cmd (correlated by (child, tool_use_id))
            ToolSlot {
                tool_use_id: "tu_child".into(),
                ordinal: 1,
                kind: SlotKind::Child(42),
                state: SlotState::Pending { cmd: None },
                result: None,
            },
            // ordinal 2 — Human slot: carries a cmd
            ToolSlot {
                tool_use_id: "tu_human".into(),
                ordinal: 2,
                kind: SlotKind::Human,
                state: SlotState::Pending { cmd: Some(8) },
                result: None,
            },
            // ordinal 3 — Peer slot: carries a cmd; its System is deferred
            ToolSlot {
                tool_use_id: "tu_peer".into(),
                ordinal: 3,
                kind: SlotKind::Peer,
                state: SlotState::Done,
                result: Some(tool_result),
            },
        ];

        let activity = Activity::ResolvingToolUses {
            slots: slots.clone(),
        };

        // Serde round-trip
        let json = serde_json::to_string(&activity).expect("serialise activity");
        let back: Activity = serde_json::from_str(&json).expect("deserialise activity");
        assert_eq!(activity, back);

        // Ordinals are preserved in order
        if let Activity::ResolvingToolUses { slots: back_slots } = &back {
            let ordinals: Vec<u16> = back_slots.iter().map(|s| s.ordinal).collect();
            assert_eq!(ordinals, vec![0, 1, 2, 3]);

            // Invariant 16: Child slot carries cmd: None
            assert_eq!(
                back_slots[1].state,
                SlotState::Pending { cmd: None },
                "Child slot must carry cmd: None (identity-correlated)"
            );

            // Invariant 16: Local slot carries Some(cmd)
            assert_eq!(
                back_slots[0].state,
                SlotState::Pending { cmd: Some(7) },
                "Local slot must carry Some(cmd)"
            );
        } else {
            panic!("expected ResolvingToolUses after round-trip");
        }
    }

    #[test]
    fn compacting_and_cancelling_round_trip() {
        let compacting = Activity::Compacting { cmd: 99 };
        let json = serde_json::to_string(&compacting).expect("serialise Compacting");
        let back: Activity = serde_json::from_str(&json).expect("deserialise Compacting");
        assert_eq!(compacting, back);

        let cancelling = Activity::Cancelling {
            awaiting: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&cancelling).expect("serialise Cancelling");
        let back: Activity = serde_json::from_str(&json).expect("deserialise Cancelling");
        assert_eq!(cancelling, back);
    }
}
