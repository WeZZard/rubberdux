//! budget — the per-entity token budget: TWO DISTINCT accounts, `spend` and
//! `context`, that must never be conflated. The cumulative `spend` drives a hard
//! per-entity HALT (BudgetSystem); the single-request `context` occupancy is the
//! compaction guard's concern. Conflating the two is the classic bug — a context
//! overflow is NOT a spend overflow. See docs/agent/world/ecs-runtime.md
//! (Two budgets: spend (halt) vs context window (compact); BudgetSystem).

use serde::{Deserialize, Serialize};

use super::gates::GuardrailTrip;
use super::inputs::Usage;

/// The stable `GuardrailTrip` a SPEND-budget halt stands on. The NAMED clearing
/// input is the gate task's `ClearPolicyHalt { trip, .. }` carrying THIS exact
/// trip — reusing the existing clearing path rather than adding a near-duplicate
/// input (Invariant 15: every halt has a named clearing input, so no budget halt
/// is a permanent sink). See docs/agent/world/ecs-runtime.md (BudgetSystem;
/// EntityGate clearing).
pub fn budget_exhausted_trip() -> GuardrailTrip {
    GuardrailTrip("budget-exhausted".into())
}

/// CUMULATIVE token spend across the whole trajectory — monotonically growing as
/// BudgetSystem folds each `ModelResponded.meta.usage`. Crossing the spend ceiling
/// HALTS the entity: a hard ceiling on TOTAL cost. DISTINCT from `Context`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spend {
    /// Cumulative input+output tokens charged to this entity so far.
    pub used: u32,
}

/// SINGLE-REQUEST context-window occupancy — the input size the next `CallModel`
/// would carry. Unlike `Spend` it does NOT grow monotonically (compaction shrinks
/// it). Crossing the context ceiling is the compaction guard's concern
/// (P2-compaction) — NEVER a spend halt. DISTINCT from `Spend`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// The most recent call's token occupancy (REFRESHED, not accumulated).
    pub used: u32,
}

/// The DISTINCT ceilings + boundedness caps a per-entity `Budget` carries. Every
/// cap follows the SAME `0 ⇒ unbounded` convention (the brake is simply not
/// configured), so a fresh/default entity is unbraked on every axis. The three
/// boundedness caps (`loop_cap`/`fanout_cap`/`inbox_capacity`) join the two budget
/// ceilings here so all of an entity's ceilings live in one place (Invariant 11 —
/// every loop, queue, and growth carries an explicit, replay-deterministic brake).
/// See docs/agent/world/ecs-runtime.md (Boundedness and backpressure).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// The cumulative-spend ceiling; `0` ⇒ unbounded.
    pub spend_limit: u32,
    /// The single-request context ceiling; `0` ⇒ unbounded.
    pub context_limit: u32,
    /// Max turns IntakeSystem may INITIATE for an entity before the loop guard
    /// forces ONE final wrap-up turn and then stops; `0` ⇒ unbounded. Checked against
    /// the per-entity turn counter (`Components.turns`). See
    /// docs/agent/world/ecs-runtime.md (Loop guard — bound the tool/turn loop).
    #[serde(default)]
    pub loop_cap: u32,
    /// Max sub-agents an entity may EVER spawn (cumulative) before a further spawn is
    /// DENIED inline with an `is_error` `ToolResult` (Inv 10), like the depth cap; `0`
    /// ⇒ unbounded. Checked against the CUMULATIVE `Components.spawned` counter — bumped
    /// on each successful spawn and NEVER decremented on child completion — so a finished
    /// child does NOT refund budget (SubagentSystem). See
    /// docs/agent/world/ecs-runtime.md (Fan-out budget — bound sub-agent spawning).
    #[serde(default)]
    pub fanout_cap: u32,
    /// Max queued messages the per-entity `Inbox` may hold; `0` ⇒ unbounded. On an
    /// enqueue past it SteeringSystem evicts the OLDEST (DropOldest backpressure) and
    /// emits a stratum-2 `MessageDropped` notice. See docs/agent/world/ecs-runtime.md
    /// (Bounded queues — Inbox DropOldest).
    #[serde(default)]
    pub inbox_capacity: u32,
}

/// A per-entity token budget holding TWO DISTINCT accounts — a cumulative `spend`
/// that drives a hard per-entity HALT, and a single-request `context` occupancy
/// the compaction guard acts on — plus their `limits`. The SPEND budget and the
/// CONTEXT budget are distinct quantities with distinct responses (halt vs.
/// compact). See docs/agent/world/ecs-runtime.md (Two budgets: spend vs context).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Cumulative spend account (→ per-entity halt on exhaustion).
    pub spend: Spend,
    /// Single-request context account (→ compaction concern, never a halt here).
    pub context: Context,
    /// The two distinct ceilings driving halt vs. compaction.
    pub limits: Limits,
}

impl Budget {
    /// Fold one inference's `Usage` into the two accounts, returning the next
    /// `Budget` (pure; saturating so it stays total). `spend` ACCUMULATES the
    /// call's input+output tokens (cumulative cost); `context` is REFRESHED to
    /// that same occupancy (single-request, non-monotonic). The two stay DISTINCT.
    pub fn fold_usage(&self, usage: Usage) -> Budget {
        let tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        Budget {
            spend: Spend {
                used: self.spend.used.saturating_add(tokens),
            },
            context: Context { used: tokens },
            limits: self.limits,
        }
    }

    /// Whether the cumulative SPEND has crossed its ceiling — the halt predicate.
    /// A `spend_limit` of 0 means unbounded, so it never reports exhausted; the
    /// CONTEXT account never influences this (the two budgets are distinct).
    pub fn spend_exhausted(&self) -> bool {
        self.limits.spend_limit != 0 && self.spend.used >= self.limits.spend_limit
    }

    /// Whether the single-request CONTEXT occupancy has crossed its ceiling —
    /// the compaction-guard predicate read by `CompactionSystem` (P2-compaction).
    /// A `context_limit` of 0 means unbounded (never exceeded); the SPEND account
    /// never influences this (the two budgets are distinct — never conflate them).
    /// See docs/agent/world/ecs-runtime.md (Context-window compaction).
    pub fn context_exceeded(&self) -> bool {
        self.limits.context_limit != 0 && self.context.used >= self.limits.context_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_accumulates_spend_but_refreshes_context() {
        let b0 = Budget {
            limits: Limits {
                spend_limit: 1000,
                context_limit: 500,
                ..Default::default()
            },
            ..Budget::default()
        };

        // First fold: 30 + 20 = 50 tokens into both accounts.
        let b1 = b0.fold_usage(Usage {
            input_tokens: 30,
            output_tokens: 20,
        });
        assert_eq!(b1.spend.used, 50);
        assert_eq!(b1.context.used, 50);

        // Second fold: 10 + 5 = 15 tokens — spend ACCUMULATES, context REFRESHES.
        let b2 = b1.fold_usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
        });
        assert_eq!(b2.spend.used, 65, "spend is cumulative across calls");
        assert_eq!(b2.context.used, 15, "context is the single-request occupancy");
        assert_eq!(
            b2.limits,
            Limits {
                spend_limit: 1000,
                context_limit: 500,
                ..Default::default()
            },
            "limits are preserved across folds"
        );
    }

    #[test]
    fn context_exceeded_is_distinct_from_spend_and_unbounded_at_zero() {
        // Unbounded by default (context_limit 0) — never exceeded.
        assert!(!Budget::default().context_exceeded());

        // A spend-heavy, context-light budget is NOT context-exceeded.
        let spend_heavy = Budget {
            spend: Spend { used: 9_999 },
            context: Context { used: 10 },
            limits: Limits { spend_limit: 1_000, context_limit: 100, ..Default::default() },
        };
        assert!(
            !spend_heavy.context_exceeded(),
            "spend occupancy never drives the CONTEXT check"
        );

        // Crossing the context ceiling IS exceeded.
        let exceeded = Budget {
            context: Context { used: 101 },
            limits: Limits { spend_limit: 0, context_limit: 100, ..Default::default() },
            ..Budget::default()
        };
        assert!(exceeded.context_exceeded(), "context.used >= context_limit is exceeded");

        // Exactly at the limit IS exceeded (>= semantics).
        let at_limit = Budget {
            context: Context { used: 100 },
            limits: Limits { spend_limit: 0, context_limit: 100, ..Default::default() },
            ..Budget::default()
        };
        assert!(at_limit.context_exceeded(), "context.used == context_limit counts as exceeded");
    }

    #[test]
    fn spend_exhausted_is_distinct_from_context_and_unbounded_at_zero() {
        // Unbounded by default (spend_limit 0) — never exhausted, so a default
        // entity does not halt on its first response.
        assert!(!Budget::default().spend_exhausted());

        // A context-heavy, spend-light budget is NOT spend-exhausted: the context
        // account never drives the spend halt (the two budgets are distinct).
        let context_heavy = Budget {
            spend: Spend { used: 10 },
            context: Context { used: 9_999 },
            limits: Limits {
                spend_limit: 1000,
                context_limit: 100,
                ..Default::default()
            },
        };
        assert!(
            !context_heavy.spend_exhausted(),
            "context occupancy never drives the SPEND halt"
        );

        // Crossing the spend ceiling IS exhaustion.
        let exhausted = Budget {
            spend: Spend { used: 1000 },
            context: Context { used: 0 },
            limits: Limits {
                spend_limit: 1000,
                context_limit: 0,
                ..Default::default()
            },
        };
        assert!(exhausted.spend_exhausted(), "used >= spend_limit is exhaustion");
    }

    #[test]
    fn fold_saturates_rather_than_overflowing() {
        let near_max = Budget {
            spend: Spend { used: u32::MAX - 1 },
            ..Budget::default()
        };
        let folded = near_max.fold_usage(Usage {
            input_tokens: 10,
            output_tokens: 10,
        });
        assert_eq!(folded.spend.used, u32::MAX, "spend saturates, never wraps");
    }

    #[test]
    fn budget_round_trips() {
        let budget = Budget {
            spend: Spend { used: 123 },
            context: Context { used: 45 },
            limits: Limits {
                spend_limit: 1000,
                context_limit: 500,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&budget).expect("serialise");
        let back: Budget = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(budget, back);
    }
}
