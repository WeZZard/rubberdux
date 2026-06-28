//! peer — the PeerDriveSystem: owns the `Peer` slot kind of a `ResolvingToolUses`
//! turn (phase 5 TurnAdvance) AND folds inbound cross-World drives. See
//! docs/agent/world/ecs-runtime.md (PeerDriveSystem; SlotKind::Peer; DriveRequested).
//!
//! A `Peer` slot models a cross-World DRIVE: the agent asked to operate ANOTHER
//! App's World as a `Peer`. PeerDriveSystem mirrors SubagentSystem's slot lifecycle
//! and is a pure function of `(World, Input)` with three roles:
//!
//! - **Emit / deny (OUTBOUND)** — on the branching `ModelResponded` (the tick
//!   TurnSystem opened the `Peer` slots in phase 3), for each fresh `Peer` slot it
//!   reads the `drive_peer` `ToolUse` block, mints a `cmd`, emits one
//!   `Command::SendPeer { cmd, to, payload, key }`, and records that `cmd` in the
//!   slot's `Pending { cmd: Some(cmd) }` (slot identity is its `cmd`, Inv 16). A
//!   malformed `drive_peer` request is DENIED inline with an `is_error` result
//!   WITHOUT emitting (Inv 10 — a peer drive that cannot be addressed never leaves
//!   the parent waiting), mirroring SubagentSystem's depth-cap deny.
//! - **Settle (OUTBOUND)** — on the sender-local `PeerSendOutcome { cmd, .. }` (the
//!   SOLE dual of `SendPeer`, Theme 4b) it settles the `Peer` slot whose `Pending`
//!   `cmd` matches: `Delivered`/`Queued` → a success `ToolResult`, `Rejected` → an
//!   `is_error` `ToolResult`. A durable send is NOT recallable, so its owed outcome
//!   is absorbed on arrival. Then it shares the all-slots-`Done` continuation with
//!   ToolSystem via `tool::advance`.
//! - **Drive (INBOUND)** — on a `DriveRequested` (origin Peer, bound to
//!   `edge_for(Counterpart::Peer(from))`) it AUTHORIZES the drive, then applies
//!   `drive.surface_ops` to the surface view as a PROJECTION of that input (no
//!   separate `SurfaceMutated` record — the `DriveRequested` log entry IS the
//!   record, Theme 4b) and enqueues `drive.prompt` (if any) to the target agent's
//!   Inbox. An invalid/absent `Authorization` is REJECTED — no surface mutation, no
//!   prompt enqueued (the World is unchanged). Inbound folding is a gate-PROOF
//!   exogenous settle (Inv 13), like a human `SurfaceMutated`.

use serde::Deserialize;

use super::{Input, System};
use crate::agent::world::effects::{Command, CommandKey};
use crate::agent::world::history::{Block, History, ToolResult};
use crate::agent::world::inputs::{
    Authorization, DeliveryOutcome, DriveCommand, LogicalInput, PeerPayload,
};
use crate::agent::world::surface::{apply_surface_ops, PeerEnvelopeId};
use crate::agent::world::world::{
    Activity, CmdId, EntityId, PeerId, SlotKind, SlotState, ToolUseId, World,
};

/// PeerDriveSystem — owns the `Peer` slot kind (phase 5 of the tick) and folds
/// inbound cross-World drives. Emit/deny each fresh `Peer` slot's `SendPeer` on the
/// branching `ModelResponded`, settle it by `PeerSendOutcome { cmd }`, and apply an
/// inbound `DriveRequested` as a surface projection + Inbox enqueue.
pub struct PeerDriveSystem;

impl System for PeerDriveSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // OUTBOUND emit: the tick a `ToolUse` turn entered `ResolvingToolUses`
            // (TurnSystem, phase 3). Emit (or deny) each fresh `Peer` slot's
            // `SendPeer`, then share the all-`Done` continuation (fires now iff every
            // slot was denied and no other kind is still pending).
            LogicalInput::ModelResponded { entity, .. } => emit_or_deny(world, entity),
            // OUTBOUND settle: the sender-local delivery ack of a `SendPeer` (the SOLE
            // dual, Theme 4b). Settle the matching `Peer` slot by `cmd`, then advance.
            LogicalInput::PeerSendOutcome {
                cmd,
                entity,
                outcome,
                ..
            } => {
                let (world, resolved) = settle_peer_outcome(world, entity, *cmd, *outcome);
                if resolved {
                    super::tool::advance(&world, entity)
                } else {
                    (world, Vec::new())
                }
            }
            // INBOUND drive: another App's lead drives THIS World. Dedup by the
            // sender-stable `envelope` at the receiver's STRATUM-1 log (effectively-
            // once), then authorize, project the surface ops, and enqueue the prompt
            // — or reject an invalid auth.
            LogicalInput::DriveRequested {
                from,
                envelope,
                drive,
                auth,
            } => apply_drive(world, from, envelope, auth, drive),
            // INBOUND generic message: another App delivered a non-drive message.
            // Recorded into the receiver's stratum-1 dedup set so a redelivery is a
            // NO-OP trace (effectively-once across BOTH inbound peer input kinds).
            LogicalInput::PeerDelivered { envelope, .. } => apply_delivered(world, envelope),
            _ => (world.clone(), Vec::new()),
        }
    }
}

/// The `drive_peer` `ToolUse` input shape (OUTBOUND): the peer `to` address and the
/// `payload` to deliver. Read from the spawning assistant block by `tool_use_id`,
/// just as SubagentSystem reads the sub-agent prompt from its `ToolUse` block.
#[derive(Deserialize)]
struct PeerDriveRequest {
    to: PeerId,
    payload: PeerPayload,
}

/// Emit (or deny) every fresh `Peer` slot of `entity`'s `ResolvingToolUses` turn,
/// then share the all-slots-`Done` continuation. Mirrors SubagentSystem's
/// `spawn_or_deny` and ToolSystem's `advance` for the `Local` kind.
fn emit_or_deny(world: &World, entity: &EntityId) -> (World, Vec<Command>) {
    // Gate guard (Inv 13): emitting a `SendPeer` is NEW WORK, so it requires BOTH the
    // App-wide `WorldGate` and the entity's `EntityGate` Open. Settling a returned
    // outcome (`PeerSendOutcome`) and folding an inbound `DriveRequested` are gate-proof.
    if !gates_open(world, entity) {
        return (world.clone(), Vec::new());
    }

    // Snapshot the `Peer` slots still needing an emit decision (Peer kind, still
    // `Pending { cmd: None }`) BEFORE mutating, releasing the read-borrow first.
    let pending: Vec<ToolUseId> = match world.entities.get(entity) {
        Some(e) => match &e.activity {
            Activity::ResolvingToolUses { slots } => slots
                .iter()
                .filter(|s| {
                    matches!(s.kind, SlotKind::Peer)
                        && matches!(s.state, SlotState::Pending { cmd: None })
                })
                .map(|s| s.tool_use_id.clone())
                .collect(),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    if pending.is_empty() {
        return (world.clone(), Vec::new());
    }

    let mut world = world.clone();
    let mut commands = Vec::new();

    for tool_use_id in pending {
        // Read the `to`/`payload` from the `drive_peer` block (the payload lives on
        // the assistant turn recorded in History; the slot carries only the
        // `tool_use_id`). The read-borrow ends before the mint/insert below.
        let request = world
            .entities
            .get(entity)
            .and_then(|e| peer_drive_request(&e.history, &tool_use_id));
        let Some((to, payload)) = request else {
            // DENY (Inv 10): a `drive_peer` whose request cannot be addressed resolves
            // the slot inline `is_error` WITHOUT emitting — MIRRORING the depth-cap
            // deny — so the parent never waits on a send that will never happen.
            resolve_slot(
                &mut world,
                entity,
                &tool_use_id,
                peer_error_result(&tool_use_id, "invalid drive_peer request"),
            );
            continue;
        };
        let (cmd, ids) = world.resources.ids.mint_cmd();
        world.resources.ids = ids;
        commands.push(Command::SendPeer {
            cmd,
            to,
            payload,
            key: CommandKey,
        });
        // Record the emitted `cmd` in the slot so the matching `PeerSendOutcome`
        // settles it (Inv 16). A durable send is not recallable; its owed outcome is
        // absorbed on arrival.
        if let Some(e) = world.entities.get_mut(entity)
            && let Activity::ResolvingToolUses { slots } = &mut e.activity
            && let Some(slot) = slots.iter_mut().find(|s| s.tool_use_id == tool_use_id)
        {
            slot.state = SlotState::Pending { cmd: Some(cmd) };
        }
    }

    // Share the all-slots-`Done` continuation with ToolSystem (fires now iff every
    // `Peer` slot was denied and no other kind is still pending). `advance` re-checks
    // the gate and only emits for slots still `Pending { cmd: None }`, so calling it
    // after ToolSystem already ran this tick is idempotent.
    let (world, mut cont) = super::tool::advance(&world, entity);
    commands.append(&mut cont);
    (world, commands)
}

/// Settle a `PeerSendOutcome` into the matching `Peer` slot of `entity`: find the
/// slot whose `Pending { cmd: Some(c) }` equals the outcome's `cmd` (slot identity is
/// its `cmd`, Inv 16) and set it `Done`, carrying a success result for
/// `Delivered`/`Queued` and an `is_error` result for `Rejected`. Returns the settled
/// World and whether a slot was resolved (so the caller advances only on a change).
fn settle_peer_outcome(
    world: &World,
    entity: &EntityId,
    cmd: CmdId,
    outcome: DeliveryOutcome,
) -> (World, bool) {
    let mut world = world.clone();
    let mut resolved = false;
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| {
            matches!(s.kind, SlotKind::Peer)
                && matches!(s.state, SlotState::Pending { cmd: Some(c) } if c == cmd)
        })
    {
        let tuid = slot.tool_use_id.clone();
        slot.result = Some(peer_outcome_result(&tuid, outcome));
        slot.state = SlotState::Done;
        resolved = true;
    }
    (world, resolved)
}

/// Fold an inbound `DriveRequested` (INBOUND): DEDUP by the sender-stable
/// `envelope` at the receiver's STRATUM-1 log, then authorize the drive, apply its
/// `surface_ops` as a PROJECTION of this input (no separate `SurfaceMutated`, Theme
/// 4b), enqueue its `prompt` to the target agent's Inbox, AND record the envelope
/// into `Resources.applied_envelopes` as part of the same fold. An invalid/absent
/// `Authorization` is REJECTED — the World is returned UNCHANGED (no mutation, no
/// enqueue, no record). Emits no Commands: the surface fold and the Inbox enqueue
/// are pure state, and IntakeSystem (gated) decides when the enqueued prompt
/// initiates a turn.
///
/// The dedup authority is `Resources.applied_envelopes` (NOT a broker-side ledger
/// written before the receiver applies): a redelivery whose `envelope` is already
/// in the set is a NO-OP trace — never re-applied. Because the record is created BY
/// this fold (atomic with the World-log append the driver performs), a crash BEFORE
/// the fold leaves NO dedup, so a redelivery re-folds — at-least-once transport plus
/// this idempotency is effectively-once, with no crash window that both dedups and
/// drops a message. See docs/agent/world/ecs-runtime.md §"Durable peer delivery".
fn apply_drive(
    world: &World,
    from: &PeerId,
    envelope: &PeerEnvelopeId,
    auth: &Authorization,
    drive: &DriveCommand,
) -> (World, Vec<Command>) {
    // STRATUM-1 DEDUP (INV-1/INV-2): an envelope already folded is a NO-OP trace —
    // never re-folded. This is the SOLE dedup authority; there is no pre-apply
    // broker ledger that could suppress a redelivery the effectively-once guarantee
    // depends on.
    if world.resources.applied_envelopes.contains(envelope) {
        log::trace!(
            "[peer-drive] DriveRequested envelope {} already folded; deduped (not re-applied)",
            envelope.0
        );
        return (world.clone(), Vec::new());
    }

    // AUTHORIZATION (P0 shape-level, design §558–562): the totality requirement is
    // that the transition EXISTS and is gated by a token; WHO may drive and how the
    // token is verified is a later enforcement pass. P0 rejects an absent/empty token
    // OR a token whose asserted `from` does not match the envelope's sender. A
    // rejected drive applies NOTHING and is NOT recorded as folded, so a later
    // authorized retry of the same envelope is still evaluated.
    if !authorized(from, auth) {
        return (world.clone(), Vec::new());
    }

    let mut next = world.clone();
    // Project the surface ops as a fold of THIS input (the agent-equivalent of the
    // App-edge `set_value` projection), under the same conflict policy. No second
    // `SurfaceMutated` is authored — the `DriveRequested` log entry is the sole record.
    if !drive.surface_ops.is_empty() {
        let (surfaces, _outcomes) =
            apply_surface_ops(&next.resources.surfaces, &drive.surface_ops);
        next.resources.surfaces = surfaces;
    }
    // Enqueue the prompt to the App's primary agent's Inbox (B10) so the target
    // processes it on its next turn initiation (IntakeSystem drains the Inbox FIFO).
    if let Some(prompt) = &drive.prompt {
        let root = next.root;
        if let Some(entity) = next.entities.get_mut(&root) {
            entity.inbox.pending.push(vec![Block::Text {
                text: prompt.clone(),
            }]);
        }
    }
    // Record the envelope AS PART OF the fold (atomic with the projection above):
    // replay reconstructs the applied set by re-folding the same EXOGENOUS input, so
    // a redelivery is deduped ONLY because this envelope was already applied.
    next.resources
        .applied_envelopes
        .insert(envelope.clone());
    (next, Vec::new())
}

/// Fold an inbound `PeerDelivered` (INBOUND, generic non-drive message): record its
/// `envelope` into the receiver's STRATUM-1 dedup set so a redelivery is a NO-OP
/// trace (effectively-once across BOTH inbound peer input kinds). A generic peer
/// message has no surface/Inbox projection in P0 — recording the envelope IS the
/// whole fold, keeping the dedup authority complete. Emits no Commands.
/// See docs/agent/world/ecs-runtime.md §"Durable peer delivery".
fn apply_delivered(world: &World, envelope: &PeerEnvelopeId) -> (World, Vec<Command>) {
    if world.resources.applied_envelopes.contains(envelope) {
        log::trace!(
            "[peer-drive] PeerDelivered envelope {} already folded; deduped (not re-applied)",
            envelope.0
        );
        return (world.clone(), Vec::new());
    }
    let mut next = world.clone();
    next.resources
        .applied_envelopes
        .insert(envelope.clone());
    (next, Vec::new())
}

/// Whether the inbound drive's `Authorization` passes the P0 shape-level check
/// (design §558–562): a NON-empty token whose asserted `from` matches the envelope's
/// `from`. The verification semantics (who may issue it, how it is checked) are a
/// later enforcement pass; this is only the existence + shape of the gated transition.
fn authorized(from: &PeerId, auth: &Authorization) -> bool {
    !auth.token.is_empty() && &auth.from == from
}

/// Read the `drive_peer` request (`to`, `payload`) from the `ToolUse` block of
/// `tool_use_id` in `history`. Returns `None` when the block is absent or its input
/// does not deserialize into a `PeerDriveRequest` — the signal to DENY the slot.
fn peer_drive_request(history: &History, tool_use_id: &str) -> Option<(PeerId, PeerPayload)> {
    history
        .0
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            Block::ToolUse { id, input, .. } if id == tool_use_id => {
                serde_json::from_value::<PeerDriveRequest>(input.clone())
                    .ok()
                    .map(|r| (r.to, r.payload))
            }
            _ => None,
        })
}

/// Resolve the `Peer` slot of `entity` whose `tool_use_id` matches: carry `result`
/// and set it `Done` — the deny path's inline resolution (Inv 10). Mirrors
/// SubagentSystem's `resolve_slot`.
fn resolve_slot(world: &mut World, entity: &EntityId, tool_use_id: &str, result: ToolResult) {
    if let Some(e) = world.entities.get_mut(entity)
        && let Activity::ResolvingToolUses { slots } = &mut e.activity
        && let Some(slot) = slots.iter_mut().find(|s| s.tool_use_id == tool_use_id)
    {
        slot.result = Some(result);
        slot.state = SlotState::Done;
    }
}

/// The `ToolResult` a settled `Peer` slot carries for a delivery `outcome`, stamped
/// to the slot's `tool_use_id` so the assembled `tool_result` pairs with its
/// `tool_use`: `Delivered`/`Queued` → a success result; `Rejected` → `is_error`
/// (the peer's inbox was full, the sender sees the rejection — Inv 10).
fn peer_outcome_result(tool_use_id: &str, outcome: DeliveryOutcome) -> ToolResult {
    let (text, is_error) = match outcome {
        DeliveryOutcome::Delivered => ("peer drive delivered", false),
        DeliveryOutcome::Queued => ("peer drive queued for offline peer", false),
        DeliveryOutcome::Rejected => ("peer drive rejected (peer inbox full)", true),
    };
    Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: vec![Block::Text {
            text: text.to_string(),
        }],
        is_error,
    }
}

/// Build an `is_error` `ToolResult` for a DENIED `Peer` slot (a malformed
/// `drive_peer` request), stamped to the slot's `tool_use_id` so the assembled
/// `tool_result` pairs with its `tool_use` (Inv 10).
fn peer_error_result(tool_use_id: &str, message: &str) -> ToolResult {
    Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: vec![Block::Text {
            text: message.to_string(),
        }],
        is_error: true,
    }
}

/// Whether BOTH the App-wide `WorldGate` and `entity`'s `EntityGate` are Open — the
/// new-work gate guard a `SendPeer` emit must pass (Inv 13). A missing entity is
/// treated as closed. Mirrors SubagentSystem's `gates_open`.
fn gates_open(world: &World, entity: &EntityId) -> bool {
    let entity_gate_open = world
        .entities
        .get(entity)
        .map(|e| e.gate.is_open())
        .unwrap_or(false);
    world.resources.gate.is_open() && entity_gate_open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::budget::Budget;
    use crate::agent::world::effects::Command;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{History, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy,
        StopReason, Usage,
    };
    use crate::agent::world::surface::{PeerEnvelopeId, SurfaceOp};
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig,
        Resources, SlotKind, SlotState, ToolSlot, ToolUseId, World,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// A World with a single primary entity in the given `activity` and `history`.
    fn world_with(activity: Activity, history: History) -> World {
        let mut world = World::new(0, Resources::new(42, model()));
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage {
                    parent: None,
                    depth: 0,
                },
                history,
                activity,
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );
        world
    }

    fn peer(node: &str) -> PeerId {
        PeerId {
            app_id: "app-b".into(),
            node_id: node.into(),
        }
    }

    /// A `drive_peer` `ToolUse` block whose input addresses `to` and carries a drive
    /// payload of one `SetValue` op plus a prompt.
    fn drive_peer_block(tool_use_id: &str, to: &PeerId, prompt: &str) -> Block {
        Block::ToolUse {
            id: tool_use_id.into(),
            name: "drive_peer".into(),
            input: serde_json::json!({
                "to": { "app_id": to.app_id, "node_id": to.node_id },
                "payload": {
                    "kind": "drive",
                    "surface_ops": [
                        { "op": "set_value", "surface": 9, "element": 2, "value": "x", "base_version": null }
                    ],
                    "prompt": prompt,
                }
            }),
        }
    }

    /// One `Peer` `ToolSlot`, `Pending { cmd }`, for `tool_use_id`.
    fn peer_slot(tool_use_id: &str, cmd: Option<CmdId>) -> ToolSlot {
        ToolSlot {
            tool_use_id: tool_use_id.into(),
            ordinal: 0,
            kind: SlotKind::Peer,
            state: SlotState::Pending { cmd },
            result: None,
        }
    }

    fn model_responded(entity: EntityId, at: u64) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd: 0,
                entity,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: "claude-x".into(),
                    stop_reason: StopReason::ToolUse,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    fn peer_send_outcome(cmd: CmdId, entity: EntityId, outcome: DeliveryOutcome) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at: 5,
            wall: None,
            input: LogicalInput::PeerSendOutcome {
                cmd,
                entity,
                fingerprint: Fingerprint("fp".into()),
                to: peer("n1"),
                outcome,
            },
        }
    }

    fn drive_requested(from: &PeerId, surface_ops: Vec<SurfaceOp>, prompt: Option<&str>, token: &str) -> Event {
        Event {
            origin: Origin::Peer,
            edge: 2,
            at: 7,
            wall: None,
            input: LogicalInput::DriveRequested {
                from: from.clone(),
                envelope: PeerEnvelopeId("env-1".into()),
                drive: DriveCommand {
                    surface_ops,
                    prompt: prompt.map(|s| s.to_string()),
                },
                auth: Authorization {
                    from: from.clone(),
                    token: token.into(),
                },
            },
        }
    }

    fn slots_of(world: &World, entity: EntityId) -> Vec<ToolSlot> {
        match &world.entities.get(&entity).expect("entity").activity {
            Activity::ResolvingToolUses { slots } => slots.clone(),
            other => panic!("expected ResolvingToolUses, got {other:?}"),
        }
    }

    /// OUTBOUND emit: a fresh `Peer` slot on the branching `ModelResponded` emits
    /// exactly one `Command::SendPeer` (carrying the `to`/`payload` decoded from the
    /// `drive_peer` block) and opens the slot `Pending { cmd: Some(cmd) }` (Inv 16).
    #[test]
    fn outbound_emits_send_peer_and_opens_the_peer_slot() {
        let to = peer("n1");
        let history = History(vec![Msg {
            role: Role::Assistant,
            content: vec![drive_peer_block("tu_peer", &to, "drive the peer")],
        }]);
        let world = world_with(
            Activity::ResolvingToolUses {
                slots: vec![peer_slot("tu_peer", None)],
            },
            history,
        );

        let (next, commands) = PeerDriveSystem.step(&world, &model_responded(0, 2).input);

        // Exactly one SendPeer, addressed to the decoded peer with a Drive payload.
        let sends: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::SendPeer { .. }))
            .collect();
        assert_eq!(sends.len(), 1, "one SendPeer per fresh Peer slot");
        let cmd = match sends[0] {
            Command::SendPeer { cmd, to: dst, payload, .. } => {
                assert_eq!(dst, &to, "SendPeer is addressed to the decoded peer");
                assert!(
                    matches!(payload, PeerPayload::Drive(_)),
                    "the drive_peer payload decodes to a Drive"
                );
                *cmd
            }
            _ => unreachable!(),
        };

        // The slot now carries the emitted cmd (its identity for the later outcome).
        let slots = slots_of(&next, 0);
        assert_eq!(slots.len(), 1);
        assert_eq!(
            slots[0].state,
            SlotState::Pending { cmd: Some(cmd) },
            "the Peer slot is opened Pending cmd: Some(cmd) (Inv 16)"
        );
    }

    /// OUTBOUND settle (Delivered): a `PeerSendOutcome` matched by `cmd` settles the
    /// `Peer` slot `Done` with a SUCCESS result, and — being the only slot — fires the
    /// shared `tool::advance` continuation (→ Thinking, one CallModel).
    #[test]
    fn outbound_peer_send_outcome_settles_slot_and_continues() {
        let history = History(vec![
            Msg {
                role: Role::User,
                content: vec![Block::Text { text: "go".into() }],
            },
            Msg {
                role: Role::Assistant,
                content: vec![drive_peer_block("tu_peer", &peer("n1"), "drive")],
            },
        ]);
        let world = world_with(
            Activity::ResolvingToolUses {
                slots: vec![peer_slot("tu_peer", Some(5))],
            },
            history,
        );

        let (next, commands) =
            PeerDriveSystem.step(&world, &peer_send_outcome(5, 0, DeliveryOutcome::Delivered).input);

        // The continuation fired: the parent advanced to a fresh Thinking turn.
        let e = next.entities.get(&0).expect("entity");
        let cont = match e.activity {
            Activity::Thinking { cmd } => cmd,
            ref other => panic!("expected Thinking continuation, got {other:?}"),
        };
        let calls: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { entity: 0, .. }))
            .collect();
        assert_eq!(calls.len(), 1, "exactly one continuation CallModel");
        match calls[0] {
            Command::CallModel { cmd, .. } => assert_eq!(*cmd, cont),
            _ => unreachable!(),
        }
        // The assembled tool_result pairs with the slot's tool_use_id, not an error.
        match &e.history.0.last().expect("result msg").content[0] {
            Block::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, "tu_peer");
                assert!(!is_error, "a Delivered outcome is a success result");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// OUTBOUND settle (Rejected): a `Rejected` outcome settles the slot `Done` with an
    /// `is_error` result (the peer's inbox was full) — still non-blocking (Inv 10).
    #[test]
    fn outbound_rejected_outcome_settles_is_error() {
        let world = world_with(
            Activity::ResolvingToolUses {
                slots: vec![peer_slot("tu_peer", Some(5))],
            },
            History::default(),
        );

        let (next, _commands) =
            PeerDriveSystem.step(&world, &peer_send_outcome(5, 0, DeliveryOutcome::Rejected).input);

        // The only slot settled `Done`, so the continuation fired (→ Thinking) and the
        // assembled tool_result carries the `is_error` rejection.
        let e = next.entities.get(&0).expect("entity");
        assert!(matches!(e.activity, Activity::Thinking { .. }), "all slots Done → continuation");
        match &e.history.0.last().expect("result msg").content[0] {
            Block::ToolResult { is_error, tool_use_id, .. } => {
                assert!(*is_error, "a Rejected peer send rides as is_error (Inv 10)");
                assert_eq!(tool_use_id, "tu_peer");
            }
            other => panic!("expected an is_error ToolResult, got {other:?}"),
        }
    }

    /// OUTBOUND deny: a `drive_peer` whose input cannot be decoded into a peer request
    /// is DENIED inline `is_error` WITHOUT emitting a `SendPeer` (Inv 10 — no deadlock).
    #[test]
    fn outbound_malformed_request_is_denied_without_emitting() {
        let history = History(vec![Msg {
            role: Role::Assistant,
            content: vec![Block::ToolUse {
                id: "tu_peer".into(),
                name: "drive_peer".into(),
                // No `to`/`payload` → cannot be addressed.
                input: serde_json::json!({ "garbage": true }),
            }],
        }]);
        let world = world_with(
            Activity::ResolvingToolUses {
                slots: vec![peer_slot("tu_peer", None)],
            },
            history,
        );

        let (next, commands) = PeerDriveSystem.step(&world, &model_responded(0, 2).input);

        assert!(
            !commands.iter().any(|c| matches!(c, Command::SendPeer { .. })),
            "a malformed drive_peer emits no SendPeer"
        );
        // The slot resolved is_error and (being the only slot) the continuation fired.
        let e = next.entities.get(&0).expect("entity");
        assert!(matches!(e.activity, Activity::Thinking { .. }), "all slots Done → continuation");
        match &e.history.0.last().expect("result msg").content[0] {
            Block::ToolResult { tool_use_id, is_error, .. } => {
                assert_eq!(tool_use_id, "tu_peer");
                assert!(*is_error, "a malformed drive_peer resolves is_error (Inv 10)");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// INBOUND (VC-3.1): a valid-auth `DriveRequested` applies its `surface_ops` as a
    /// projection (bumping the surface version) and enqueues its `prompt` to the root
    /// agent's Inbox — emitting no Commands.
    #[test]
    fn inbound_drive_projects_surface_ops_and_enqueues_prompt() {
        let world = world_with(Activity::Idle, History::default());
        let from = peer("remote");
        let ops = vec![SurfaceOp::SetValue {
            surface: 9,
            element: 2,
            value: serde_json::json!("typed by peer"),
            base_version: None,
        }];

        let (next, commands) =
            PeerDriveSystem.step(&world, &drive_requested(&from, ops, Some("please confirm"), "tok").input);

        assert!(commands.is_empty(), "an inbound drive emits no Commands");
        // The surface op applied as a projection (no separate SurfaceMutated): 0→1.
        assert_eq!(
            next.resources.surfaces.get(&9).map(|s| s.version),
            Some(1),
            "the drive's surface_ops apply as a projection, bumping the version 0→1"
        );
        // The prompt enqueued to the root agent's Inbox.
        assert_eq!(
            next.entities.get(&0).expect("root").inbox.pending,
            vec![vec![Block::Text { text: "please confirm".into() }]],
            "the drive's prompt is enqueued to the Inbox"
        );
    }

    /// INBOUND stratum-1 dedup (VC-4.2, acceptance (a)): a redelivered
    /// `DriveRequested` whose `envelope` was ALREADY folded is a NO-OP — the World
    /// is unchanged (the surface version is NOT bumped a second time, the prompt is
    /// NOT enqueued twice) and the applied-envelope set still holds it exactly once.
    /// The receiver's stratum-1 log is the SOLE dedup authority (no broker ledger),
    /// so at-least-once redelivery folds to effectively-once.
    #[test]
    fn inbound_redelivered_envelope_is_not_refolded() {
        let world = world_with(Activity::Idle, History::default());
        let from = peer("remote");
        let ops = vec![SurfaceOp::SetValue {
            surface: 9,
            element: 2,
            value: serde_json::json!("typed by peer"),
            base_version: None,
        }];

        // First delivery: the envelope folds (surface 0→1, prompt enqueued, envelope
        // recorded in the stratum-1 dedup set).
        let event = drive_requested(&from, ops, Some("please confirm"), "tok");
        let (once, _c1) = PeerDriveSystem.step(&world, &event.input);
        assert_eq!(once.resources.surfaces.get(&9).map(|s| s.version), Some(1));
        assert_eq!(once.entities.get(&0).expect("root").inbox.pending.len(), 1);
        assert!(
            once.resources
                .applied_envelopes
                .contains(&PeerEnvelopeId("env-1".into())),
            "the first fold records the envelope in the stratum-1 dedup set"
        );

        // Redelivery of the SAME envelope (an at-least-once retry, or a
        // crash-interrupted earlier drain): a NO-OP trace, never re-applied.
        let (twice, c2) = PeerDriveSystem.step(&once, &event.input);
        assert!(c2.is_empty(), "a deduped redelivery emits no Commands");
        assert_eq!(
            &twice, &once,
            "a redelivered envelope is a NO-OP: the World is byte-unchanged \
             (no second surface bump, no second prompt) — effectively-once"
        );
        assert_eq!(
            twice.resources.surfaces.get(&9).map(|s| s.version),
            Some(1),
            "the surface version is NOT bumped a second time by the redelivery"
        );
        assert_eq!(
            twice.entities.get(&0).expect("root").inbox.pending.len(),
            1,
            "the prompt is NOT enqueued a second time by the redelivery"
        );
        assert_eq!(
            twice.resources.applied_envelopes.len(),
            1,
            "the envelope is recorded exactly once"
        );
    }

    /// INBOUND stratum-1 dedup for a generic message (acceptance (a), `PeerDelivered`
    /// arm): a redelivered `PeerDelivered` whose `envelope` was already folded is a
    /// NO-OP — the dedup authority covers BOTH inbound peer input kinds.
    #[test]
    fn inbound_redelivered_peer_message_is_not_refolded() {
        let world = world_with(Activity::Idle, History::default());
        let delivered = Event {
            origin: Origin::Peer,
            edge: 2,
            at: 7,
            wall: None,
            input: LogicalInput::PeerDelivered {
                from: peer("remote"),
                envelope: PeerEnvelopeId("msg-1".into()),
                payload: serde_json::json!({ "text": "hi" }),
            },
        };

        let (once, _c1) = PeerDriveSystem.step(&world, &delivered.input);
        assert!(
            once.resources
                .applied_envelopes
                .contains(&PeerEnvelopeId("msg-1".into())),
            "a fresh PeerDelivered records its envelope"
        );

        let (twice, c2) = PeerDriveSystem.step(&once, &delivered.input);
        assert!(c2.is_empty());
        assert_eq!(&twice, &once, "a redelivered PeerDelivered is a NO-OP (deduped)");
        assert_eq!(twice.resources.applied_envelopes.len(), 1);
    }

    /// INBOUND (VC-3.3, empty token): a `DriveRequested` with an ABSENT/empty
    /// `Authorization` token is REJECTED — no surface mutation, no prompt enqueued,
    /// the World is unchanged.
    #[test]
    fn inbound_drive_with_empty_token_is_rejected() {
        let world = world_with(Activity::Idle, History::default());
        let from = peer("remote");
        let ops = vec![SurfaceOp::SetValue {
            surface: 9,
            element: 2,
            value: serde_json::json!("x"),
            base_version: None,
        }];

        let (next, commands) =
            PeerDriveSystem.step(&world, &drive_requested(&from, ops, Some("do x"), "").input);

        assert!(commands.is_empty());
        assert!(next.resources.surfaces.is_empty(), "no surface mutation on rejection");
        assert!(
            next.entities.get(&0).expect("root").inbox.pending.is_empty(),
            "no prompt enqueued on rejection"
        );
        assert_eq!(&next, &world, "the World is unchanged on an invalid-auth rejection");
    }

    /// INBOUND (VC-3.3, mismatched sender): a `DriveRequested` whose `auth.from` does
    /// NOT match the envelope's `from` is REJECTED — the shape-level transition exists
    /// (design §558–562) and an inconsistent authorization is denied.
    #[test]
    fn inbound_drive_with_mismatched_auth_from_is_rejected() {
        let world = world_with(Activity::Idle, History::default());
        let from = peer("remote");
        // The drive arrives from `remote`, but the auth asserts a DIFFERENT sender.
        let mismatched = Event {
            origin: Origin::Peer,
            edge: 2,
            at: 7,
            wall: None,
            input: LogicalInput::DriveRequested {
                from: from.clone(),
                envelope: PeerEnvelopeId("env-1".into()),
                drive: DriveCommand {
                    surface_ops: vec![SurfaceOp::SetValue {
                        surface: 9,
                        element: 2,
                        value: serde_json::json!("x"),
                        base_version: None,
                    }],
                    prompt: Some("do x".into()),
                },
                auth: Authorization {
                    from: peer("imposter"),
                    token: "tok".into(),
                },
            },
        };

        let (next, commands) = PeerDriveSystem.step(&world, &mismatched.input);

        assert!(commands.is_empty());
        assert_eq!(&next, &world, "a mismatched auth.from is rejected, World unchanged");
    }
}
