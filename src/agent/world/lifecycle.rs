//! lifecycle — see docs/agent/world/ecs-runtime.md
//!
//! Stratum-2 OPERATIONAL records. Stratum 1 (`LogicalInput` inside an `Event`)
//! is the replay-defining logical log; stratum 2 is operational trace that is
//! NEVER folded into the World on replay. Most stratum-2 records are
//! behaviorally NEUTRAL (`Tombstoned`/`Restored`/`WorkerCrashed`/
//! `WorkerRespawned`/`MessageDropped` — trace/observability only). The ONE
//! exception is `CommandDispatched`: it is still neutral on replay, but it is
//! LOAD-BEARING FOR RECOVERY — the write-ahead dispatch-intent the live driver
//! appends BEFORE an effectful Command, carrying the durable `cmd → ctx` index,
//! the idempotency `key`, and the request `fingerprint`. The replay driver
//! ignores it; only the resume/recovery path reads it (a separate milestone).
//! See docs/agent/world/ecs-runtime.md (*Intent before commitment*; Inv 5/8/17).

use serde::{Deserialize, Serialize};

use super::inputs::{Fingerprint, Origin};
use super::world::{CmdId, EdgeId, EntityId, Tick, WallClock};

// ---------------------------------------------------------------------------
// AppId / EffectId — the components of an IdempotencyKey not held by a Command
// ---------------------------------------------------------------------------

/// App process identity — the scope component of an [`IdempotencyKey`]. It is
/// supervisor-owned (the supervisor mints it; see docs/app/supervisor.md) and
/// threaded onto a dispatch-intent so a restart-retry from a DIFFERENT App
/// process cannot collide with this App's keys. Modelled minimally as an opaque
/// string newtype; the supervisor wiring that populates it lands with the resume
/// path. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

/// The intra-tick ORDINAL of an effectful Command within the step that emits it
/// (the n-th effectful Command this tick). Deliberately NOT a persisted counter:
/// resetting it each tick keeps `IdempotencyKey = (app_id, tick, effect_id)`
/// unique without extra state. Assigned by the driver from the final emitted
/// Command list. See docs/agent/world/ecs-runtime.md (*Deterministic ids*).
pub type EffectId = u32;

// ---------------------------------------------------------------------------
// EffectKind / DropReason — small classifiers carried by lifecycle records
// ---------------------------------------------------------------------------

/// Which effect a dispatch-intent stands for. Selects the resume reconciliation
/// branch on recovery. `SendPeer`/`ScheduleTimer` are listed ahead of their
/// Commands so the kind set is stable as the live driver gains those effects.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    CallModel,
    RunTool,
    RequestHumanAction,
    SendPeer,
    ScheduleTimer,
    Compact,
}

/// Why a [`LifecycleEvent::MessageDropped`] notice fired (Theme 5d). P0/P1a has
/// the single `InboxOverflow` cause — `DropOldest` at the Inbox cap
/// (Boundedness). See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// The Inbox is at its cap; the oldest queued message was dropped.
    InboxOverflow,
}

// ---------------------------------------------------------------------------
// ActorCtx — the durable, per-`cmd` cmd → ctx routing/causal index (Theme 1b)
// ---------------------------------------------------------------------------

/// Actor/routing context attached to every entity-scoped dispatch (Theme 1b):
/// WHO the command acts for (`entity`), under WHICH causal origin (`origin`), on
/// WHICH counterpart relationship (`edge`). Recorded on
/// [`LifecycleEvent::CommandDispatched`] so every result Input inherits its
/// `entity`/`origin`/`edge` from the matching dispatch — result routing then has
/// a durable source and a derived UI effect stays causally attributable (Inv 17).
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorCtx {
    pub entity: EntityId,
    pub origin: Origin,
    pub edge: EdgeId,
}

// ---------------------------------------------------------------------------
// IdempotencyKey — the restart-retry dedup token (app_id, tick, effect_id)
// ---------------------------------------------------------------------------

/// The dedup token a restart-retry re-presents so an effect commits
/// effectively-once (= at-least-once + idempotency). Unique per effect: App
/// scope × dispatch tick × intra-tick effect ordinal. See
/// docs/agent/world/ecs-runtime.md (*Intent before commitment*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdempotencyKey {
    pub app_id: AppId,
    pub tick: Tick,
    pub effect_id: EffectId,
}

// ---------------------------------------------------------------------------
// LifecycleEvent — Stratum 2 operational records (neutral on replay)
// ---------------------------------------------------------------------------

/// A stratum-2 operational record. None of these are folded into the World on
/// replay (the two-strata convention). The four `Worker*`/`Tombstoned`/
/// `Restored` and `MessageDropped` variants are behaviorally NEUTRAL trace; only
/// `CommandDispatched` is load-bearing for recovery — and even it is read ONLY
/// by the resume path, never by the replay fold. See
/// docs/agent/world/ecs-runtime.md (Stratum 2; *Intent before commitment*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LifecycleEvent {
    /// The worker was tombstoned (idle-evicted). Neutral on replay.
    Tombstoned { at: Tick },
    /// A tombstoned worker was restored. Neutral on replay.
    Restored { at: Tick },
    /// The worker crashed. Neutral on replay — the crash's BEHAVIORAL effect is
    /// realized separately as a stratum-1 Input (the synthesized
    /// `InferenceCancelled { reason: Crash }` or `is_error` `ToolResult`).
    WorkerCrashed { at: Tick, err: String },
    /// A crashed worker was respawned. Neutral on replay.
    WorkerRespawned { at: Tick },
    /// Observable drop notice (Theme 5d): the human sees the loss, yet it never
    /// folds into the World on replay (NOT a stratum-1 Logical Input). Stamped by
    /// observed wall-time, not the logical ordering axis.
    MessageDropped {
        at: WallClock,
        edge: EdgeId,
        reason: DropReason,
        dropped: u32,
    },
    /// Write-ahead dispatch-intent — the ONE stratum-2 record that is
    /// LOAD-BEARING FOR RECOVERY. The live driver appends it BEFORE performing an
    /// effectful Command. Carries the `ctx` (the durable `cmd → ctx` index every
    /// result inherits from — Theme 1b/Inv 17), the idempotency `key`
    /// (restart-retry dedup token), and the request `fingerprint` (binds the
    /// eventual result back to this intent). The replay driver ignores it; only
    /// the resume path reads it. See docs/agent/world/ecs-runtime.md.
    CommandDispatched {
        at: Tick,
        cmd: CmdId,
        kind: EffectKind,
        ctx: ActorCtx,
        key: IdempotencyKey,
        fingerprint: Fingerprint,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(event: &LifecycleEvent) {
        let json = serde_json::to_string(event).expect("serialise lifecycle event");
        let back: LifecycleEvent = serde_json::from_str(&json).expect("deserialise lifecycle event");
        assert_eq!(event, &back);
    }

    #[test]
    fn command_dispatched_round_trips_with_ctx_key_and_fingerprint() {
        let event = LifecycleEvent::CommandDispatched {
            at: 5,
            cmd: 3,
            kind: EffectKind::CallModel,
            ctx: ActorCtx {
                entity: 0,
                origin: Origin::Agent,
                edge: 9,
            },
            key: IdempotencyKey {
                app_id: AppId("app-1".into()),
                tick: 5,
                effect_id: 0,
            },
            fingerprint: Fingerprint("fp-req-1".into()),
        };
        // The dispatch-intent must survive a serde round-trip verbatim — it is the
        // durable record the resume path reads on recovery.
        match &event {
            LifecycleEvent::CommandDispatched { key, ctx, .. } => {
                assert_eq!(key.tick, 5);
                assert_eq!(key.effect_id, 0);
                assert_eq!(ctx.edge, 9);
                assert_eq!(ctx.origin, Origin::Agent);
            }
            other => panic!("expected CommandDispatched, got {other:?}"),
        }
        round_trip(&event);
    }

    #[test]
    fn message_dropped_round_trips() {
        round_trip(&LifecycleEvent::MessageDropped {
            at: WallClock {
                observed: Some(1_700_000_000),
            },
            edge: 0,
            reason: DropReason::InboxOverflow,
            dropped: 3,
        });
    }

    #[test]
    fn neutral_lifecycle_records_round_trip() {
        round_trip(&LifecycleEvent::Tombstoned { at: 1 });
        round_trip(&LifecycleEvent::Restored { at: 2 });
        round_trip(&LifecycleEvent::WorkerCrashed {
            at: 3,
            err: "boom".into(),
        });
        round_trip(&LifecycleEvent::WorkerRespawned { at: 4 });
    }
}
