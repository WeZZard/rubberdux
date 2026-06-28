//! surface — see docs/agent/world/ecs-runtime.md

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use super::effects::{Command, ToolSet};
use super::history::{Block, ToolSchema};
use super::inputs::{Fingerprint, LogicalInput};
use super::systems::{Input, System};
use super::world::{CmdId, World};
use crate::error::Error;

// ---------------------------------------------------------------------------
// Primitive surface identifiers and versioning
// ---------------------------------------------------------------------------

/// A surface within the macOS app. Type alias over `u32`; exact identifier
/// scheme fixed by the SurfaceSystem pass. See docs/agent/world/ecs-runtime.md.
pub type SurfaceId = u32;

/// An element within a surface (AX node or logical UI element). Type alias
/// over `u32`; exact identifier scheme fixed by the SurfaceSystem pass. See
/// docs/agent/world/ecs-runtime.md.
pub type ElementId = u32;

/// JSON value used as a surface element's current value. Opaque at this pass;
/// the element-value encoding is fixed when SurfaceSystem defines the element
/// model. See docs/agent/world/ecs-runtime.md.
pub type Value = Json;

/// A (column, row) pixel point within a surface, used as an optional
/// hit-test override on `SurfaceOp::Click`. See docs/agent/world/ecs-runtime.md.
pub type Point = (u32, u32);

/// Per-surface optimistic-concurrency monotone counter (Theme 2c). Bumped on
/// every applied surface change; a `SurfaceOp.base_version` is checked
/// against it before the op is applied — an op with a stale `base_version`
/// is rejected `is_error` rather than clobbering a changed surface. See
/// docs/agent/world/ecs-runtime.md.
pub type SurfaceVersion = u64;

// ---------------------------------------------------------------------------
// Opaque shape-only types — encodings fixed by later passes
// ---------------------------------------------------------------------------

/// A content digest of the AX tree at one observation tick (Theme 2b). The
/// UI-tool `Fingerprint` folds this in so a re-emitted UI request diverges
/// exactly when the perceived UI state changed. Exact encoding fixed by the
/// SurfaceSystem pass. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hash(pub String);

/// A navigation route within a surface (e.g. a view path on a navigation
/// stack). Opaque at this pass; encoding fixed by the protocol pass. See
/// docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route(pub String);

/// A component tree to render into a surface region. Opaque at this pass;
/// encoding fixed by the macOS app pass. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentSpec(pub String);

/// An AX/UI selection state (start element, end element, character range).
/// Opaque at this pass; encoding fixed by the observation-reporting pass.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection(pub String);

/// A viewport descriptor (visible rect within a scrollable surface region).
/// Opaque at this pass; encoding fixed by the observation-reporting pass.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport(pub String);

/// A window state descriptor (frame, minimized, fullscreen, key/main status).
/// Opaque at this pass; encoding fixed by the observation-reporting pass.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState(pub String);

// ---------------------------------------------------------------------------
// IdempotencyKey — P0 placeholder (full shape arrives with P2-wal)
// ---------------------------------------------------------------------------

/// P0 placeholder for the WAL-milestone idempotency key stamped on a
/// `Cause::Command` echo from the native UI change signal. The full shape
/// `{ app_id, tick, effect_id }` (which uniquely identifies the dispatched
/// effect across crash-resume cycles) arrives with the P2-wal task. See
/// docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyKey(pub String);

// ---------------------------------------------------------------------------
// PeerEnvelopeId — sender-assigned, retry-stable cross-process delivery key
// ---------------------------------------------------------------------------

/// The sender-assigned, retry-stable cross-process delivery key for a peer
/// message.  This is the idempotency handle the receiver's stratum-1 log
/// dedups by (see docs/agent/world/ecs-runtime.md §"Durable peer delivery"):
///
/// - **Stable across retries.** The sender must assign the SAME id to every
///   attempt to deliver the same logical message.  A redelivered envelope
///   whose `PeerEnvelopeId` is already present in the receiver log is
///   deduplicated (logged as trace, never re-applied), giving
///   effectively-once semantics despite at-least-once transport.
///
/// - **Distinct per logical message.** Each distinct logical send MUST use a
///   unique id so independent messages are never silently collapsed.
///
/// - **Does not cross from per-cmd fingerprint.** The per-`cmd`
///   `Fingerprint`/`IdempotencyKey` live inside one process and are NOT
///   transmitted to the receiver; `PeerEnvelopeId` is the SOLE
///   cross-process correlation handle.
///
/// The inner string is opaque at this level; the canonical encoding is fixed
/// by the federation pass.  Use [`PeerEnvelopeId::new`] to build a key with
/// a documented stability contract.
///
/// The `Cause::Peer` echo-dedup use (Theme 1e) remains a valid secondary
/// use: correlating a native UI change echo with the originating
/// `DriveRequested` so the echo is dropped rather than logged as a redundant
/// `SurfaceMutated`.  That use is fully preserved by this type.
///
/// See docs/agent/world/ecs-runtime.md §"Durable peer delivery".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PeerEnvelopeId(pub String);

impl PeerEnvelopeId {
    /// Build a sender-assigned delivery key from the sender's `app_id`,
    /// `node_id`, and a sender-local monotone sequence number.
    ///
    /// **Stability contract:** the same `(app_id, node_id, seq)` triple
    /// always produces the same `PeerEnvelopeId`, so the sender can
    /// re-construct the identical key on every retry of the same logical
    /// send.
    ///
    /// **Uniqueness contract:** `seq` must be distinct for each distinct
    /// logical message within the same sender so independent sends are never
    /// silently deduplicated at the receiver.
    ///
    /// This method is the CANONICAL path for constructing a delivery key; the
    /// resulting id is opaque to the receiver (the receiver deduplicates by
    /// identity, not by parsing the inner string).
    pub fn new(app_id: &str, node_id: &str, seq: u64) -> Self {
        PeerEnvelopeId(format!("{app_id}/{node_id}/{seq}"))
    }
}

// ---------------------------------------------------------------------------
// SurfaceOp — payload-bearing surface mutation (Theme 2a)
// ---------------------------------------------------------------------------

/// A payload-bearing surface op (Theme 2a). Each variant names exactly what
/// it manipulates — `surface`, `element`, and the op-specific payload — so a
/// lead agent can reference the target precisely in both local `set_value`
/// tool calls and cross-World `DriveCommand.surface_ops`. The optional
/// `base_version` (Theme 2c) is the optimistic-concurrency precondition: an
/// op whose `base_version` no longer matches the current `SurfaceVersion` is
/// rejected with an `is_error` result rather than clobbering a changed
/// surface. See docs/agent/world/ecs-runtime.md (SurfaceOp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SurfaceOp {
    /// Set the value of a surface element (e.g. text-field contents, slider
    /// position). `value` is the new JSON-encoded element value.
    SetValue {
        surface: SurfaceId,
        element: ElementId,
        value: Value,
        base_version: Option<SurfaceVersion>,
    },
    /// Synthesise a click on a surface element. `point` overrides the
    /// element's default hit-test point when `Some`.
    Click {
        surface: SurfaceId,
        element: ElementId,
        point: Option<Point>,
        base_version: Option<SurfaceVersion>,
    },
    /// Navigate the surface to a new route (e.g. push a new view onto a
    /// navigation stack).
    Navigate {
        surface: SurfaceId,
        route: Route,
        base_version: Option<SurfaceVersion>,
    },
    /// Render a component tree into a surface region.
    Render {
        surface: SurfaceId,
        component: ComponentSpec,
        base_version: Option<SurfaceVersion>,
    },
}

// ---------------------------------------------------------------------------
// Cause — the origin the OS attaches to a native UI change signal (Theme 1e)
// ---------------------------------------------------------------------------

/// The cause the OS attaches to a native UI change signal — the basis of
/// echo dedup (Theme 1e). Only `Human` is logged as a stratum-1
/// `SurfaceMutated` input. `Command`/`Peer` signals are the screen's own
/// echoes of an already-logged `ToolReturned`/`DriveRequested` and are
/// DROPPED by the shell so an agent write is not double-counted and the mode
/// projection does not flap. See docs/agent/world/ecs-runtime.md (Cause).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Cause {
    /// The change originated from direct human interaction with the UI.
    /// Only this variant is logged as a stratum-1 `SurfaceMutated` input.
    Human,
    /// The change is the screen's echo of an agent-dispatched `set_value`
    /// UI `RunTool` — identified by the originating command `cmd` and the
    /// idempotency `key`. DROP in the shell; the agent write is already
    /// recorded as `ToolReturned`.
    Command {
        cmd: CmdId,
        key: IdempotencyKey,
    },
    /// The change is the screen's echo of a cross-World `DriveRequested` —
    /// identified by the sender's durable `envelope`. DROP in the shell; the
    /// drive is already recorded as `DriveRequested`.
    Peer {
        envelope: PeerEnvelopeId,
    },
}

// ---------------------------------------------------------------------------
// SurfaceState — latest observed state of one surface (minimal stub)
// ---------------------------------------------------------------------------

/// The latest observed state of one surface, stored in `Resources.surfaces`
/// keyed by `SurfaceId`. Fields mirror the `SurfaceObserved` logical-input
/// payload (minus the surface key). `SurfaceSystem` (task U-surface-system)
/// fills this map when it folds `SurfaceObserved` inputs; until that System
/// is implemented the map is empty and this struct is a minimal stub. See
/// docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceState {
    /// The last observed surface version (optimistic-concurrency counter).
    pub version: SurfaceVersion,
    /// Content digest of the AX tree at the last observation tick.
    pub ax_digest: Hash,
    /// The currently focused element, if any.
    pub focus: Option<ElementId>,
    /// The current AX/UI selection state, if any.
    pub selection: Option<Selection>,
    /// The current viewport descriptor.
    pub viewport: Viewport,
    /// The current window state.
    pub window: WindowState,
    /// The cursor position within the surface, if known.
    pub cursor: Option<Point>,
}

// ---------------------------------------------------------------------------
// SurfaceView — deterministic per-surface map stored in Resources
// ---------------------------------------------------------------------------

/// Deterministic map from `SurfaceId` to its latest observed `SurfaceState`.
/// `BTreeMap` is required (Invariant 8): a random-order map's iteration would
/// be a hidden input that diverges `World'` on replay. Empty until the first
/// `SurfaceObserved` input is folded by `SurfaceSystem`. See
/// docs/agent/world/ecs-runtime.md.
pub type SurfaceView = BTreeMap<SurfaceId, SurfaceState>;

// ---------------------------------------------------------------------------
// SurfaceState construction — a present-but-unobserved surface
// ---------------------------------------------------------------------------

impl SurfaceState {
    /// A surface PRESENT in the view but not yet observed: `SurfaceVersion` 0 and
    /// every opaque perception field empty. Used when a `SurfaceOp` targets a
    /// surface before any `SurfaceObserved` has been folded, so its optimistic-
    /// concurrency `SurfaceVersion` is still tracked monotonically from the first
    /// applied change. See docs/agent/world/ecs-runtime.md.
    fn unobserved() -> SurfaceState {
        SurfaceState {
            version: 0,
            ax_digest: Hash(String::new()),
            focus: None,
            selection: None,
            viewport: Viewport(String::new()),
            window: WindowState(String::new()),
            cursor: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SurfaceOp accessors — the target surface and the concurrency precondition
// ---------------------------------------------------------------------------

impl SurfaceOp {
    /// The `SurfaceId` this op targets — every variant names a `surface`.
    pub fn surface(&self) -> SurfaceId {
        match self {
            SurfaceOp::SetValue { surface, .. }
            | SurfaceOp::Click { surface, .. }
            | SurfaceOp::Navigate { surface, .. }
            | SurfaceOp::Render { surface, .. } => *surface,
        }
    }

    /// The optimistic-concurrency precondition (Theme 2c): when `Some(v)` the op
    /// applies ONLY if the target surface's current `SurfaceVersion` is exactly
    /// `v`; `None` means "no precondition".
    pub fn base_version(&self) -> Option<SurfaceVersion> {
        match self {
            SurfaceOp::SetValue { base_version, .. }
            | SurfaceOp::Click { base_version, .. }
            | SurfaceOp::Navigate { base_version, .. }
            | SurfaceOp::Render { base_version, .. } => *base_version,
        }
    }
}

// ---------------------------------------------------------------------------
// Conflict policy — LastWriterWins with an optimistic-concurrency precondition
// ---------------------------------------------------------------------------

/// The outcome of applying ONE `SurfaceOp` under the conflict policy (Theme 2c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    /// The op applied; the target surface's `SurfaceVersion` advanced to `version`.
    Applied {
        surface: SurfaceId,
        version: SurfaceVersion,
    },
    /// The op was REJECTED by the optimistic-concurrency precondition: its
    /// `base_version` no longer matched the surface's current `SurfaceVersion`.
    /// An executor turns this into an `is_error` `ToolResult` (see
    /// `rejection_tool_result`).
    Rejected {
        surface: SurfaceId,
        base_version: SurfaceVersion,
        current: SurfaceVersion,
    },
}

impl OpOutcome {
    /// Whether this outcome is the rejected (`is_error`) case.
    pub fn is_error(&self) -> bool {
        matches!(self, OpOutcome::Rejected { .. })
    }
}

/// Apply a batch of `surface_ops` to a `SurfaceView` under the conflict policy
/// (Theme 2c): ops apply in `Vec` ORDER (`LastWriterWins` — a later op on the
/// same surface supersedes an earlier one by bumping the version again); an op
/// whose `base_version` no longer matches the target surface's current
/// `SurfaceVersion` is REJECTED (optimistic concurrency) without mutating the
/// view; and each APPLIED change BUMPS that surface's `SurfaceVersion` by one.
/// Pure: a function of the prior view and the ops only. Returns the next view
/// and the per-op outcomes in `Vec` order. See docs/agent/world/ecs-runtime.md
/// (SurfaceSystem; CONFLICT POLICY).
pub fn apply_surface_ops(surfaces: &SurfaceView, ops: &[SurfaceOp]) -> (SurfaceView, Vec<OpOutcome>) {
    let mut next = surfaces.clone();
    let mut outcomes = Vec::with_capacity(ops.len());
    for op in ops {
        let surface = op.surface();
        let current = next.get(&surface).map(|s| s.version).unwrap_or(0);
        match op.base_version() {
            // Stale precondition — reject without clobbering the changed surface.
            Some(base) if base != current => {
                outcomes.push(OpOutcome::Rejected {
                    surface,
                    base_version: base,
                    current,
                });
            }
            // No precondition, or it matches — apply and bump the version.
            _ => {
                let state = next.entry(surface).or_insert_with(SurfaceState::unobserved);
                state.version = current + 1;
                outcomes.push(OpOutcome::Applied {
                    surface,
                    version: state.version,
                });
            }
        }
    }
    (next, outcomes)
}

/// Build the `is_error` `ToolResult` an executor returns for a `set_value`/UI
/// `RunTool` when one or more of its `surface_ops` were rejected by the
/// optimistic-concurrency precondition. Returns `None` when nothing was rejected
/// (the call succeeded). `tool_use_id` binds the result back to the originating
/// `ToolUse` block. See docs/agent/world/ecs-runtime.md (CONFLICT POLICY).
pub fn rejection_tool_result(tool_use_id: &str, outcomes: &[OpOutcome]) -> Option<Block> {
    let rejected: Vec<String> = outcomes
        .iter()
        .filter_map(|o| match o {
            OpOutcome::Rejected {
                surface,
                base_version,
                current,
            } => Some(format!(
                "surface {surface}: stale base_version {base_version} (current {current})"
            )),
            OpOutcome::Applied { .. } => None,
        })
        .collect();
    if rejected.is_empty() {
        return None;
    }
    Some(Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: vec![Block::Text {
            text: format!("rejected stale surface ops: {}", rejected.join("; ")),
        }],
        is_error: true,
    })
}

// ---------------------------------------------------------------------------
// Echo dedup — only a Human native signal becomes a SurfaceMutated (Theme 1e)
// ---------------------------------------------------------------------------

/// How the shell must treat a native UI change signal once it reads the OS's
/// `Cause` (Theme 1e). See `classify_native_signal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSignal {
    /// Log this native signal as a stratum-1, human-origin `SurfaceMutated` Input
    /// (the ONLY `SurfaceMutated` ever logged).
    LogAsMutation,
    /// DROP this signal: it is the screen's own echo of an already-logged
    /// `ToolReturned` (an agent `set_value`) or `DriveRequested` (a peer drive),
    /// so logging a `SurfaceMutated` would double-count the same write.
    DropEcho,
}

/// The echo-dedup RULE (Theme 1e / Inv 18): classify a native UI change signal
/// by the `Cause` the OS attached. ONLY `Cause::Human` becomes a stratum-1
/// `SurfaceMutated`; `Cause::Command`/`Cause::Peer` are echoes of an
/// already-logged `ToolReturned`/`DriveRequested` and are DROPPED, so an agent
/// (or peer) write is never double-counted and the mode projection never flaps.
/// Pure classifier; the ingest wiring that consults it is the worker task
/// (P-worker). See docs/agent/world/ecs-runtime.md (Cause; Native echo dedup).
pub fn classify_native_signal(cause: &Cause) -> NativeSignal {
    match cause {
        Cause::Human => NativeSignal::LogAsMutation,
        Cause::Command { .. } | Cause::Peer { .. } => NativeSignal::DropEcho,
    }
}

// ---------------------------------------------------------------------------
// UI-tool fingerprint — folds the perceived surface state (Inv 18)
// ---------------------------------------------------------------------------

/// Compute the request `Fingerprint` for a UI / `set_value` tool call, FOLDING
/// IN the perceived `surface_version` and `ax_digest` the manipulation depended
/// on (recorded by the latest `SurfaceObserved`). A SHA-256 over the fixed-order
/// `(tool, args, surface_version, ax_digest)` payload, so a re-emitted UI request
/// DIVERGES exactly when the perceived macOS UI state changed — closing the "UI
/// state is a hidden input" replay hole (Inv 18, Theme 2b). See
/// docs/agent/world/ecs-runtime.md.
pub fn fingerprint_ui_request(
    tool: &str,
    args: &Json,
    perceived: &SurfaceState,
) -> Result<Fingerprint, Error> {
    /// The exact, fixed-order payload that is hashed. A dedicated struct so the
    /// field order is explicit rather than positional.
    #[derive(Serialize)]
    struct Payload<'a> {
        tool: &'a str,
        args: &'a Json,
        surface_version: SurfaceVersion,
        ax_digest: &'a Hash,
    }

    let bytes = serde_json::to_vec(&Payload {
        tool,
        args,
        surface_version: perceived.version,
        ax_digest: &perceived.ax_digest,
    })?;
    let digest = Sha256::digest(&bytes);
    Ok(Fingerprint(hex::encode(digest)))
}

/// The perceived `SurfaceState` a `set_value`/UI tool's `Fingerprint` folds (Inv
/// 18 / Theme 2b): the latest observed state of the surface its `surface_ops`
/// target — the FIRST op's `surface` — defaulting to an UNOBSERVED surface
/// (version 0, empty perception) when none has been observed yet. Threading this
/// state into `fingerprint_ui_request` is what makes a re-emitted drive DIVERGE
/// exactly when the macOS UI state it depended on changed: the live driver reads
/// it from the World's `Resources.surfaces` at dispatch, so an identical request
/// over an identical observed surface re-hashes identically. See
/// docs/agent/world/ecs-runtime.md (UI-tool fingerprint).
pub fn perceived_for_ops(surfaces: &SurfaceView, ops: &[SurfaceOp]) -> SurfaceState {
    ops.first()
        .and_then(|op| surfaces.get(&op.surface()))
        .cloned()
        .unwrap_or_else(SurfaceState::unobserved)
}

// ---------------------------------------------------------------------------
// SurfaceSystem — folds surface inputs into Resources.surfaces (settle phase)
// ---------------------------------------------------------------------------

/// The projection envelope a `set_value`/UI tool's `ToolReturned` carries so the
/// applied `surface_ops` can be folded into the surface view. The agent UI
/// write's SOLE log record is that `ToolReturned` (Inv 18); the ops ride inside
/// its result as a `{ "surface_ops": [...] }` payload so `SurfaceSystem` folds
/// them WITHOUT a second `SurfaceMutated` record. The wire-side production of
/// this envelope is the worker task (P-worker). See docs/agent/world/ecs-runtime.md
/// (The agent UI-write echo is one fact).
#[derive(Deserialize)]
struct SurfaceOpsProjection {
    surface_ops: Vec<SurfaceOp>,
}

/// Decode the `surface_ops` an agent `set_value`/UI tool applied from its
/// `ToolReturned.result` blocks (the projection source — see
/// `SurfaceOpsProjection`). Walks the result blocks and any nested `tool_result`
/// content; a non-surface tool's result decodes to NO ops, so the projection is
/// a no-op for it. Pure.
pub fn surface_ops_from_tool_result(result: &[Block]) -> Vec<SurfaceOp> {
    fn collect(blocks: &[Block], out: &mut Vec<SurfaceOp>) {
        for block in blocks {
            match block {
                Block::Text { text } => {
                    if let Ok(projection) = serde_json::from_str::<SurfaceOpsProjection>(text) {
                        out.extend(projection.surface_ops);
                    }
                }
                Block::ToolResult { content, .. } => collect(content, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    collect(result, &mut out);
    out
}

/// Build the single `ToolReturned.result` block a `set_value`/UI tool's LIVE
/// dispatch records — the agent UI write's SOLE fact (Inv 18). The ops are
/// applied against the perceived `surfaces` (optimistic concurrency, Theme 2c):
///
/// - when every op applied, the result carries the `{"surface_ops":[...]}`
///   PROJECTION envelope `SurfaceSystem` decodes (`surface_ops_from_tool_result`)
///   and folds ONCE into a surface-version bump — no second `SurfaceMutated`;
/// - when the precondition REJECTED one or more ops, the `is_error` `ToolResult`
///   from `rejection_tool_result` instead (the surface is left unchanged, the
///   model sees the failure as a well-formed `tool_result`).
///
/// `tool_use_id` binds the result back to its originating `ToolUse`; the owning
/// `ToolSystem` re-stamps it to the slot's id when it settles (the executor does
/// not know the slot's id, so an empty placeholder is normal here). Pure. See
/// docs/agent/world/ecs-runtime.md (The agent UI-write echo is one fact; CONFLICT POLICY).
pub fn surface_tool_result(
    tool_use_id: &str,
    surfaces: &SurfaceView,
    ops: &[SurfaceOp],
) -> Result<Block, Error> {
    let (_applied, outcomes) = apply_surface_ops(surfaces, ops);
    if let Some(rejected) = rejection_tool_result(tool_use_id, &outcomes) {
        return Ok(rejected);
    }
    /// The fixed-shape projection envelope `surface_ops_from_tool_result` decodes
    /// (the `Serialize` dual of `SurfaceOpsProjection`).
    #[derive(Serialize)]
    struct Projection<'a> {
        surface_ops: &'a [SurfaceOp],
    }
    // The projection envelope SurfaceSystem folds into a version bump.
    let envelope = serde_json::to_string(&Projection { surface_ops: ops })?;
    Ok(Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: vec![Block::Text { text: envelope }],
        is_error: false,
    })
}

/// The Anthropic tool DECLARATION for the agent's `set_value` surface tool — the
/// one surface-manipulation tool a live model call is told EXISTS so it can request
/// it. Its `input_schema` is shaped EXACTLY to the args the live `set_value`
/// executor parses (`args["surface_ops"]` → `Vec<SurfaceOp>` in `drive_live`): an
/// object with a `surface_ops` array, each item a `SetValue` op in the canonical
/// `#[serde(tag = "op")]` encoding — `{ op: "set_value", surface, element, value,
/// base_version }`. A model `tool_use.input` built to this schema therefore flows
/// straight into the executor with no reshaping. Pure (a constant table). See
/// docs/agent/world/ecs-runtime.md (The agent UI-write echo is one fact; SurfaceOp).
pub fn set_value_tool_schema() -> ToolSchema {
    ToolSchema {
        name: "set_value".to_string(),
        description: "Set the value of one or more macOS UI surface elements \
                      (e.g. type text into a field, move a slider). Each op names \
                      the target surface and element and the new value; an optional \
                      base_version is an optimistic-concurrency precondition that \
                      applies the op only if the surface is still at that version."
            .to_string(),
        // The JSON Schema mirrors `{ "surface_ops": [ SurfaceOp::SetValue, ... ] }`,
        // the exact `args` the live `set_value` executor decodes. `value` is left
        // unconstrained (any JSON, matching `SurfaceOp.value: Json`); `base_version`
        // is an integer or null (matching `Option<SurfaceVersion>`).
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "surface_ops": {
                    "type": "array",
                    "description": "The set-value ops to apply, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "const": "set_value" },
                            "surface": {
                                "type": "integer",
                                "description": "Target surface id."
                            },
                            "element": {
                                "type": "integer",
                                "description": "Target element id within the surface."
                            },
                            "value": {
                                "description": "The new element value (any JSON)."
                            },
                            "base_version": {
                                "type": ["integer", "null"],
                                "description": "Optimistic-concurrency precondition: \
                                                apply only if the surface is still at \
                                                this version. Null or omitted means no \
                                                precondition."
                            }
                        },
                        "required": ["op", "surface", "element", "value"]
                    }
                }
            },
            "required": ["surface_ops"]
        }),
    }
}

/// SurfaceSystem — the runtime's view of client surfaces, settled in phase 3.
///
/// It folds the surface inputs into `Resources.surfaces`, all pure functions of
/// `(World, Input)`:
///
/// - a HUMAN-origin `SurfaceMutated { op }` (the only logged `SurfaceMutated`,
///   guaranteed human-origin by `classify_native_signal` before it became an
///   Input) is applied DIRECTLY under the conflict policy — a human grabbing the
///   wheel (B8);
/// - a `ToolReturned` carrying a `set_value`/UI tool's applied `surface_ops` is
///   folded as a PROJECTION of that single commit record — the agent write's
///   SOLE record is its `ToolReturned` (Inv 18), folded exactly once (this is the
///   one `ToolReturned` Input for that `cmd`), never re-authored as a separate
///   `SurfaceMutated`;
/// - a `SurfaceObserved` REFRESHES the perceived per-surface state
///   (`version`/`ax_digest`/focus/selection/viewport/window/cursor).
///
/// It also OWNS one genesis fold: a `SessionStarted` seeds the recorded App
/// `surface_tools` into `Resources.surface_tools` (the surface-domain config the
/// CallModel-emitting Systems read into each root turn's `ToolSet`). Folding it
/// from the recorded header — rather than seeding it live-only — is what makes the
/// live and replay Worlds reconstruct surface tools IDENTICALLY (Inv 6/7).
///
/// Activity is unchanged by a surface update, so it emits no Commands. See
/// docs/agent/world/ecs-runtime.md (SurfaceSystem).
pub struct SurfaceSystem;

impl System for SurfaceSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        let mut next = world.clone();
        match input {
            // Genesis: seed the App's surface tools from the recorded session
            // header — the SINGLE source of truth for `Resources.surface_tools`.
            // BOTH the live fold and every replay genesis fold this SAME recorded
            // `SessionStarted`, so the root entity's surface tools reconstruct
            // IDENTICALLY and a re-emitted root `CallModel` re-hashes to its
            // recorded `Fingerprint` (Inv 7). An empty list (a tool-less session)
            // leaves `surface_tools` empty — byte-identical to a pre-tools log.
            LogicalInput::SessionStarted { surface_tools, .. } => {
                next.resources.surface_tools = ToolSet(surface_tools.clone());
            }
            // Human grabbed the wheel — apply directly under the conflict policy.
            LogicalInput::SurfaceMutated { op } => {
                let (surfaces, _outcomes) =
                    apply_surface_ops(&next.resources.surfaces, std::slice::from_ref(op));
                next.resources.surfaces = surfaces;
            }
            // PROJECTION of the agent's set_value/UI write, folded ONCE keyed by
            // the originating `cmd` (this is its sole `ToolReturned` Input); no
            // separate `SurfaceMutated` is authored (Inv 18). A non-surface tool
            // decodes to no ops → no change.
            LogicalInput::ToolReturned { result, .. } => {
                let ops = surface_ops_from_tool_result(result);
                if !ops.is_empty() {
                    let (surfaces, _outcomes) =
                        apply_surface_ops(&next.resources.surfaces, &ops);
                    next.resources.surfaces = surfaces;
                }
            }
            // Refresh the perceived per-surface state (Theme 2b).
            LogicalInput::SurfaceObserved {
                surface,
                version,
                ax_digest,
                focus,
                selection,
                viewport,
                window,
                cursor,
            } => {
                next.resources.surfaces.insert(
                    *surface,
                    SurfaceState {
                        version: *version,
                        ax_digest: ax_digest.clone(),
                        focus: *focus,
                        selection: selection.clone(),
                        viewport: viewport.clone(),
                        window: window.clone(),
                        cursor: *cursor,
                    },
                );
            }
            _ => {}
        }
        (next, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_partial<T>(value: &T)
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialise");
        let back: T = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(value, &back);
    }

    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Eq + std::fmt::Debug,
    {
        round_trip_partial(value);
    }

    // --- SurfaceOp (each variant) ------------------------------------------

    #[test]
    fn surface_op_set_value_round_trips() {
        round_trip_partial(&SurfaceOp::SetValue {
            surface: 1,
            element: 42,
            value: serde_json::json!("new text"),
            base_version: Some(7),
        });
        // Without a base_version precondition
        round_trip_partial(&SurfaceOp::SetValue {
            surface: 1,
            element: 42,
            value: serde_json::json!({"count": 3}),
            base_version: None,
        });
    }

    #[test]
    fn surface_op_click_round_trips() {
        round_trip_partial(&SurfaceOp::Click {
            surface: 2,
            element: 10,
            point: Some((100, 200)),
            base_version: None,
        });
        // Without a point override
        round_trip_partial(&SurfaceOp::Click {
            surface: 2,
            element: 10,
            point: None,
            base_version: Some(3),
        });
    }

    #[test]
    fn surface_op_navigate_round_trips() {
        round_trip_partial(&SurfaceOp::Navigate {
            surface: 3,
            route: Route("home".into()),
            base_version: None,
        });
    }

    #[test]
    fn surface_op_render_round_trips() {
        round_trip_partial(&SurfaceOp::Render {
            surface: 4,
            component: ComponentSpec("button-bar".into()),
            base_version: Some(1),
        });
    }

    // --- Cause (each variant) -----------------------------------------------

    #[test]
    fn cause_human_round_trips() {
        round_trip(&Cause::Human);
    }

    #[test]
    fn cause_command_round_trips() {
        round_trip(&Cause::Command {
            cmd: 7,
            key: IdempotencyKey("tick-7-effect-0".into()),
        });
    }

    #[test]
    fn cause_peer_round_trips() {
        round_trip(&Cause::Peer {
            envelope: PeerEnvelopeId("env-abc-123".into()),
        });
    }

    // --- SurfaceVersion is u64 ---------------------------------------------

    #[test]
    fn surface_version_is_u64() {
        let v: SurfaceVersion = 42;
        assert_eq!(v + 1, 43);
    }

    // --- SurfaceSystem behaviour (task U-surface-system) -------------------

    use crate::agent::world::inputs::{Fingerprint, LogicalInput};
    use crate::agent::world::systems::System;
    use crate::agent::world::world::{Effort, ModelConfig, Resources, World};

    /// A fresh World with an empty surface view, ready to fold surface inputs.
    fn empty_world() -> World {
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        };
        World::new(0, Resources::new(7, model))
    }

    /// [Verifies VC-U.1] A HUMAN-origin `SurfaceMutated` is applied directly and
    /// BUMPS the target surface's `SurfaceVersion`; a surface update emits no
    /// Commands.
    #[test]
    fn human_surface_mutated_applies_directly_and_bumps_version() {
        let world = empty_world();
        let input = LogicalInput::SurfaceMutated {
            op: SurfaceOp::SetValue {
                surface: 5,
                element: 1,
                value: serde_json::json!("typed by hand"),
                base_version: None,
            },
        };
        let (next, cmds) = SurfaceSystem.step(&world, &input);
        assert!(cmds.is_empty(), "a surface update emits no Commands");
        assert_eq!(
            next.resources.surfaces.get(&5).map(|s| s.version),
            Some(1),
            "an applied human op bumps the surface version 0→1"
        );
    }

    /// [Verifies VC-U.1] [Verifies VC-U.4] An agent `set_value` `ToolReturned`
    /// folds the surface change as a projection EXACTLY ONCE (no separate
    /// `SurfaceMutated` is authored, and the System emits no Commands).
    #[test]
    fn agent_set_value_tool_returned_folds_surface_change_once() {
        let world = empty_world();
        // The set_value RunTool's ToolReturned carries its applied ops as the
        // projection envelope — the SOLE agent-write record (Inv 18).
        let envelope = serde_json::json!({
            "surface_ops": [
                { "op": "set_value", "surface": 9, "element": 2, "value": "typed", "base_version": null }
            ]
        })
        .to_string();
        let input = LogicalInput::ToolReturned {
            cmd: 3,
            entity: 0,
            fingerprint: Fingerprint("fp-ui".into()),
            result: vec![Block::ToolResult {
                tool_use_id: "tu_set_value".into(),
                content: vec![Block::Text { text: envelope }],
                is_error: false,
            }],
        };
        let (next, cmds) = SurfaceSystem.step(&world, &input);
        assert!(
            cmds.is_empty(),
            "the projection authors no SurfaceMutated and emits no Commands"
        );
        // Folded EXACTLY once: version bumped 0→1 (one op applied, not twice).
        assert_eq!(
            next.resources.surfaces.get(&9).map(|s| s.version),
            Some(1),
            "the agent write is folded once keyed by cmd, bumping the version 0→1"
        );
    }

    /// The `set_value` executor's result builder (`surface_tool_result`): when every
    /// op applies it emits the `{"surface_ops":[...]}` projection envelope that
    /// `SurfaceSystem` folds into a version bump; when an op's `base_version` is
    /// stale it emits an `is_error` `ToolResult` instead and the projection decodes
    /// to NO ops (the surface is left unchanged). `perceived_for_ops` reads the
    /// targeted surface's observed state for the fingerprint.
    #[test]
    fn surface_tool_result_projects_on_success_and_is_error_on_rejection() {
        let ops = vec![SurfaceOp::SetValue {
            surface: 9,
            element: 2,
            value: serde_json::json!("typed"),
            base_version: None,
        }];

        // Success: the envelope decodes to the op and folds to a version bump 0→1.
        let ok = surface_tool_result("", &SurfaceView::new(), &ops).expect("build result");
        assert!(matches!(&ok, Block::ToolResult { is_error: false, .. }));
        let (next, _) = SurfaceSystem.step(&empty_world(), &LogicalInput::ToolReturned {
            cmd: 1,
            entity: 0,
            fingerprint: Fingerprint("fp".into()),
            result: vec![ok],
        });
        assert_eq!(
            next.resources.surfaces.get(&9).map(|s| s.version),
            Some(1),
            "the success envelope folds to a version bump 0→1"
        );

        // Rejection: an op preconditioned on a STALE base_version yields an
        // is_error result whose content decodes to no surface_ops (no bump).
        let mut surfaces = SurfaceView::new();
        surfaces.insert(9, SurfaceState { version: 5, ..SurfaceState::unobserved() });
        let stale = vec![SurfaceOp::SetValue {
            surface: 9,
            element: 2,
            value: serde_json::json!("typed"),
            base_version: Some(3),
        }];
        let err = surface_tool_result("", &surfaces, &stale).expect("build result");
        assert!(matches!(&err, Block::ToolResult { is_error: true, .. }));
        assert!(
            surface_ops_from_tool_result(std::slice::from_ref(&err)).is_empty(),
            "a rejection result carries no surface_ops projection"
        );
    }

    /// `perceived_for_ops` reads the OBSERVED state of the surface the ops target,
    /// defaulting to an unobserved surface (version 0) when none was observed —
    /// the perceived input the UI-request fingerprint folds (Inv 18).
    #[test]
    fn perceived_for_ops_reads_the_targeted_surface_or_defaults_unobserved() {
        let ops = vec![SurfaceOp::SetValue {
            surface: 9,
            element: 2,
            value: serde_json::json!("x"),
            base_version: None,
        }];
        // No observation yet → unobserved (version 0).
        assert_eq!(perceived_for_ops(&SurfaceView::new(), &ops).version, 0);
        // Observed at version 5 → that state is folded.
        let mut surfaces = SurfaceView::new();
        surfaces.insert(9, SurfaceState { version: 5, ..SurfaceState::unobserved() });
        assert_eq!(perceived_for_ops(&surfaces, &ops).version, 5);
    }

    /// [Verifies VC-U.2] [Verifies VC-U.3] A batch applies in `Vec` order under
    /// `LastWriterWins` with each applied change bumping the version, and an op
    /// carrying a STALE `base_version` is rejected (`is_error`) without clobbering.
    #[test]
    fn stale_base_version_op_is_rejected_as_is_error() {
        // A surface already at version 5.
        let mut surfaces = SurfaceView::new();
        surfaces.insert(
            2,
            SurfaceState {
                version: 5,
                ax_digest: Hash(String::new()),
                focus: None,
                selection: None,
                viewport: Viewport(String::new()),
                window: WindowState(String::new()),
                cursor: None,
            },
        );

        // An op preconditioned on the STALE base_version 3.
        let ops = vec![SurfaceOp::SetValue {
            surface: 2,
            element: 0,
            value: serde_json::json!("x"),
            base_version: Some(3),
        }];
        let (next, outcomes) = apply_surface_ops(&surfaces, &ops);

        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].is_error(), "a stale base_version must be rejected");
        assert!(matches!(
            outcomes[0],
            OpOutcome::Rejected {
                surface: 2,
                base_version: 3,
                current: 5
            }
        ));
        assert_eq!(
            next.get(&2).map(|s| s.version),
            Some(5),
            "a rejected op must not bump (clobber) the version"
        );

        // The executor turns the rejection into an is_error ToolResult.
        let result = rejection_tool_result("tu_set_value", &outcomes)
            .expect("a rejected op yields an is_error ToolResult");
        assert!(matches!(result, Block::ToolResult { is_error: true, .. }));
    }

    /// LastWriterWins: a batch of two ops on the same surface applies in `Vec`
    /// order, each bumping the version (so the LAST op's effect is the final one).
    #[test]
    fn batched_ops_apply_in_vec_order_last_writer_wins() {
        let surfaces = SurfaceView::new();
        let ops = vec![
            SurfaceOp::SetValue {
                surface: 1,
                element: 0,
                value: serde_json::json!("first"),
                base_version: None,
            },
            SurfaceOp::SetValue {
                surface: 1,
                element: 0,
                value: serde_json::json!("second"),
                base_version: None,
            },
        ];
        let (next, outcomes) = apply_surface_ops(&surfaces, &ops);
        assert_eq!(
            outcomes,
            vec![
                OpOutcome::Applied { surface: 1, version: 1 },
                OpOutcome::Applied { surface: 1, version: 2 },
            ],
            "ops apply in Vec order, each bumping the version"
        );
        assert_eq!(next.get(&1).map(|s| s.version), Some(2));
    }

    // --- PeerEnvelopeId — sender-assigned delivery key (VC-4.2) -----------

    /// [Verifies VC-4.2] `PeerEnvelopeId::new` builds a key that is
    /// **stable across retries** (same inputs → same id) and **distinct per
    /// logical message** (different seq → different id), proving the
    /// stability contract required for effectively-once delivery.
    /// The key also round-trips through serde-canonical JSON unchanged.
    #[test]
    fn peer_envelope_id_is_stable_across_retries_and_round_trips() {
        let app_id = "2026-01-01-00-00-00-000000-000000-UTC";
        let node_id = "node-a";

        // Stability: same (app_id, node_id, seq) → identical id on every call.
        let a = PeerEnvelopeId::new(app_id, node_id, 7);
        let b = PeerEnvelopeId::new(app_id, node_id, 7);
        assert_eq!(a, b, "same inputs must produce the same delivery key (retry-stable)");

        // Uniqueness: distinct seq → distinct id (different logical messages).
        let c = PeerEnvelopeId::new(app_id, node_id, 8);
        assert_ne!(a, c, "different seq must produce a different delivery key");

        // Round-trip: serde-canonical, no HashMap, no float.
        round_trip(&a);
        round_trip(&c);

        // Distinct sender identity → distinct id even for the same seq.
        let d = PeerEnvelopeId::new("other-app", node_id, 7);
        assert_ne!(a, d, "different app_id must produce a different delivery key");
    }

    /// [Verifies VC-4.2] The existing `Cause::Peer` echo-dedup use is
    /// UNAFFECTED by the promotion: `classify_native_signal` still returns
    /// `DropEcho` for a `Cause::Peer` carrying a `PeerEnvelopeId` built via
    /// the new constructor.
    #[test]
    fn peer_envelope_id_promotion_does_not_break_echo_dedup() {
        let envelope = PeerEnvelopeId::new("2026-01-01-00-00-00-000000-000000-UTC", "node-b", 1);
        let cause = Cause::Peer { envelope };
        assert_eq!(
            classify_native_signal(&cause),
            NativeSignal::DropEcho,
            "Cause::Peer still classified as DropEcho after PeerEnvelopeId promotion"
        );
    }

    /// [Verifies VC-U.1] Echo dedup: only a `Cause::Human` native signal becomes a
    /// `SurfaceMutated`; `Cause::Command`/`Cause::Peer` echoes are DROPPED.
    #[test]
    fn echo_dedup_logs_only_human_native_signals() {
        assert_eq!(classify_native_signal(&Cause::Human), NativeSignal::LogAsMutation);
        assert_eq!(
            classify_native_signal(&Cause::Command {
                cmd: 7,
                key: IdempotencyKey("tick-7-effect-0".into()),
            }),
            NativeSignal::DropEcho,
            "a Command-caused echo of an agent set_value is dropped"
        );
        assert_eq!(
            classify_native_signal(&Cause::Peer {
                envelope: PeerEnvelopeId("env-1".into()),
            }),
            NativeSignal::DropEcho,
            "a Peer-caused echo of a DriveRequested is dropped"
        );
    }

    /// [Verifies VC-U.2] The UI-tool `Fingerprint` folds in the perceived
    /// `surface_version`/`ax_digest`: identical request + identical observed
    /// surface → identical fingerprint; bumping `surface_version` diverges it.
    #[test]
    fn ui_tool_fingerprint_folds_perceived_surface_state() {
        let perceived = SurfaceState {
            version: 4,
            ax_digest: Hash("ax-abc".into()),
            focus: None,
            selection: None,
            viewport: Viewport(String::new()),
            window: WindowState(String::new()),
            cursor: None,
        };
        let args = serde_json::json!({ "surface": 1, "element": 2, "value": "hello" });

        let a = fingerprint_ui_request("set_value", &args, &perceived).expect("fp a");
        let b = fingerprint_ui_request("set_value", &args, &perceived).expect("fp b");
        assert_eq!(
            a, b,
            "identical request + identical observed surface → identical fingerprint"
        );

        // Bump the perceived surface_version → the fingerprint DIVERGES (Inv 18).
        let mut bumped = perceived.clone();
        bumped.version += 1;
        let c = fingerprint_ui_request("set_value", &args, &bumped).expect("fp c");
        assert_ne!(
            a, c,
            "a changed perceived surface_version must diverge the fingerprint"
        );
    }

    /// A `SurfaceObserved` refreshes the perceived per-surface state in the view.
    #[test]
    fn surface_observed_refreshes_perceived_state() {
        let world = empty_world();
        let input = LogicalInput::SurfaceObserved {
            surface: 3,
            version: 11,
            ax_digest: Hash("digest-xyz".into()),
            focus: Some(2),
            selection: Some(Selection("0-4".into())),
            viewport: Viewport("rect".into()),
            window: WindowState("key".into()),
            cursor: Some((10, 20)),
        };
        let (next, cmds) = SurfaceSystem.step(&world, &input);
        assert!(cmds.is_empty());
        let state = next.resources.surfaces.get(&3).expect("surface present");
        assert_eq!(state.version, 11);
        assert_eq!(state.ax_digest, Hash("digest-xyz".into()));
        assert_eq!(state.focus, Some(2));
    }
}
