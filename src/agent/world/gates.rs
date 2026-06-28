//! gates — see docs/agent/world/ecs-runtime.md

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Gate support types
// ---------------------------------------------------------------------------

/// Why a gate holds a pause. A gate carries a SET of these holds (one per
/// independent source — a user pause and a policy halt can coexist); the gate
/// returns to `Open` only when ALL holds clear. `Resume` clears every `User`
/// hold; `ClearPolicyHalt` clears one matching `PolicyHalt` hold. See
/// docs/agent/world/ecs-runtime.md (WorldGate; Invariants 13, 15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// A user-initiated pause, cleared by `Resume`.
    User,
    /// A guardrail tripped, cleared by `ClearPolicyHalt` carrying the matching
    /// `GuardrailTrip` and a valid `Authority`.
    PolicyHalt(GuardrailTrip),
}

/// An opaque identifier for the guardrail trip that caused a `PolicyHalt`
/// pause hold. Exact encoding fixed by the gate/guardrail task. P1a
/// placeholder. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailTrip(pub String);

/// Opaque authority token that authorises clearing a `PolicyHalt` hold. Exact
/// verification semantics are an enforcement concern, fixed by a later pass.
/// P1a placeholder. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority(pub String);

impl Authority {
    /// The P0 shape-level predicate that gates a halt clear (design §567-570,
    /// §1869-1871: "Authority/authorization are SHAPE ONLY here"). A clear is
    /// AUTHORIZED iff a non-blank credential is presented: the totality
    /// requirement is that the clearing transition EXISTS and is GATED by a
    /// token, so a blank/whitespace authority is REJECTED rather than silently
    /// clearing the hold. WHO may issue the token and richer verification are a
    /// later enforcement pass. Mirrors the `PeerDriveSystem` P0 authorization
    /// (a non-empty token), keeping the gated-but-existent shape consistent.
    pub fn authorizes_clear(&self) -> bool {
        !self.0.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// WorldGate — App-wide run-state (User pause / PolicyHalt)
// ---------------------------------------------------------------------------

/// App-WIDE run-state, owned by GateSystem. It carries a SET of pause `holds`
/// (independent sources can coexist) and is OPEN iff `holds` is empty — a closed
/// WorldGate blocks turn INITIATION and CONTINUATION across the whole App but
/// NEVER result-settling (Invariant 13). `Pause` pushes a hold, `Resume` drains
/// every `User` hold, and `ClearPolicyHalt` removes the matching `PolicyHalt`
/// hold (so a policy halt survives a user resume). See
/// docs/agent/world/ecs-runtime.md (WorldGate).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldGate {
    /// The active pause holds; non-empty ⇒ the gate is closed.
    pub holds: Vec<PauseReason>,
}

impl WorldGate {
    /// The pure gate-open predicate: OPEN iff no holds remain. Consulted by the
    /// initiating/continuing Systems; result-settling never consults it (Inv 13).
    pub fn is_open(&self) -> bool {
        self.holds.is_empty()
    }
}

// ---------------------------------------------------------------------------
// EntityGate / EntityHalt — per-entity run-gate (orthogonal to the WorldGate)
// ---------------------------------------------------------------------------

/// Per-entity run-gate, orthogonal both to `Activity` (turn progress) and to the
/// App-wide `WorldGate`: it lets ONE entity be held WITHOUT freezing the World. It
/// is OPEN iff it carries no pause `holds` AND no `halt`. A transition that
/// INITIATES or CONTINUES work requires this gate (and the WorldGate) Open; a
/// result-SETTLING transition never consults it (Invariant 13). A policy
/// `EntityHalt` is cleared by its named input `ClearPolicyHalt` (Invariant 15).
/// See docs/agent/world/ecs-runtime.md (EntityGate).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityGate {
    /// Per-entity pause holds; non-empty ⇒ closed.
    pub holds: Vec<PauseReason>,
    /// A standing policy halt, if any; `Some(_)` ⇒ closed until cleared.
    pub halt: Option<EntityHalt>,
}

impl EntityGate {
    /// The pure gate-open predicate: OPEN iff no holds remain AND no halt stands.
    pub fn is_open(&self) -> bool {
        self.holds.is_empty() && self.halt.is_none()
    }
}

/// A standing per-entity halt, keyed by the `GuardrailTrip` that caused it. Its
/// NAMED clearing input is `ClearPolicyHalt { trip, authority }` carrying the
/// matching `trip` (Invariant 15 — every halt has a named clearing input, so no
/// halt is a permanent sink). See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityHalt {
    /// The guardrail trip this halt stands on; matched by `ClearPolicyHalt.trip`.
    pub trip: GuardrailTrip,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T)
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialise");
        let back: T = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(value, &back);
    }

    #[test]
    fn pause_reason_variants_round_trip() {
        round_trip(&PauseReason::User);
        round_trip(&PauseReason::PolicyHalt(GuardrailTrip("policy-42".into())));
    }

    #[test]
    fn guardrail_trip_round_trips() {
        round_trip(&GuardrailTrip("content-filter-v1".into()));
    }

    #[test]
    fn authority_round_trips() {
        round_trip(&Authority("admin-token-xyz".into()));
    }

    /// The P0 authority predicate is a CONCRETE, PURE check: a non-blank token
    /// authorizes a halt clear; an empty or whitespace-only token does not.
    #[test]
    fn authority_authorizes_clear_iff_token_is_non_blank() {
        assert!(
            Authority("admin".into()).authorizes_clear(),
            "a non-blank credential authorizes a clear"
        );
        assert!(
            !Authority(String::new()).authorizes_clear(),
            "an empty credential is rejected"
        );
        assert!(
            !Authority("   ".into()).authorizes_clear(),
            "a whitespace-only credential is insufficient"
        );
    }

    // -----------------------------------------------------------------------
    // Gate-open predicate — the pure `is_open` semantics (Inv 13, 15)
    // -----------------------------------------------------------------------

    #[test]
    fn world_gate_is_open_iff_no_holds() {
        // A fresh gate (no holds) is open.
        let mut gate = WorldGate::default();
        assert!(gate.is_open(), "a gate with no holds is open");

        // Any hold closes it.
        gate.holds.push(PauseReason::User);
        assert!(!gate.is_open(), "a held gate is closed");

        // Draining the hold reopens it.
        gate.holds.clear();
        assert!(gate.is_open(), "draining every hold reopens the gate");
    }

    #[test]
    fn entity_gate_is_open_iff_no_holds_and_no_halt() {
        let mut gate = EntityGate::default();
        assert!(gate.is_open(), "a fresh entity gate is open");

        // A pause hold closes it.
        gate.holds.push(PauseReason::User);
        assert!(!gate.is_open(), "a pause hold closes the entity gate");
        gate.holds.clear();
        assert!(gate.is_open());

        // A standing policy halt also closes it — and is open ONLY once cleared.
        gate.halt = Some(EntityHalt {
            trip: GuardrailTrip("policy-7".into()),
        });
        assert!(!gate.is_open(), "a standing halt closes the entity gate");
        gate.halt = None;
        assert!(gate.is_open(), "clearing the halt reopens the entity gate");
    }

    #[test]
    fn gate_types_round_trip() {
        round_trip(&WorldGate {
            holds: vec![
                PauseReason::User,
                PauseReason::PolicyHalt(GuardrailTrip("g1".into())),
            ],
        });
        round_trip(&EntityGate {
            holds: vec![PauseReason::User],
            halt: Some(EntityHalt {
                trip: GuardrailTrip("g2".into()),
            }),
        });
        round_trip(&EntityHalt {
            trip: GuardrailTrip("g3".into()),
        });
    }
}
