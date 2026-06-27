//! autonomy — see docs/agent/world/ecs-runtime.md

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

// ---------------------------------------------------------------------------
// Autonomy policy
// ---------------------------------------------------------------------------

/// How much the agent may do without human approval, ranging from seeking
/// approval for every action to running entirely free. The world default lives
/// in `Resources.autonomy`; an optional per-entity override in
/// `Components.autonomy` shadows it (entity override else world default).
/// See docs/agent/world/ecs-runtime.md (Autonomy/Tier).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// Seek human approval for every consequential action.
    AskEverything,
    /// Gate on a specific consequence tier. See `Tier`.
    GateTier(Tier),
    /// Run without seeking approval.
    RunFree,
}

/// Consequence tier for `Autonomy::GateTier`. Names which class of actions
/// require approval before the agent proceeds.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Both reversible and irreversible actions proceed without asking.
    FreeRun,
    /// Reversible actions proceed freely; irreversible actions are flagged for
    /// approval before execution.
    FlagReversible,
    /// Reversible actions proceed freely; irreversible actions are blocked
    /// until explicitly approved.
    BlockIrreversible,
}

// ---------------------------------------------------------------------------
// Interaction types — raised by the agent to request guidance or approval
// ---------------------------------------------------------------------------

/// An agent-facing interaction the agent raises via `RaiseInteraction`. The
/// shell surfaces this to the user or an approval authority; the response
/// arrives as `InteractionAnswer { answer: InteractionResponse }`. Minimal
/// P1a shape — exact fields fixed by a later pass.
/// See docs/agent/world/ecs-runtime.md (RaiseInteraction).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInteraction {
    /// Opaque payload for P1a; exact fields fixed by a later pass.
    pub payload: Json,
}

/// The response to a `RaiseInteraction`. Correlated to its request by the
/// stable `request_id` carried in `InteractionAnswer`. Minimal P1a shape.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionResponse {
    /// The interaction was accepted or approved.
    Accepted,
    /// The interaction was rejected or denied.
    Rejected,
    /// A structured data answer to the agent's question.
    Data(Json),
}

// ---------------------------------------------------------------------------
// Human-action types — used in RequestHumanAction / HumanActionDone
// ---------------------------------------------------------------------------

/// How the shell surfaces a `RequestHumanAction` to the user.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Notify {
    /// Push a visible notification to the user.
    Push,
    /// Enqueue silently without interrupting the user.
    Silent,
}

/// A human-action request the agent raises via `RequestHumanAction`. Each
/// variant names a kind of input the agent needs from a human. Minimal P1a
/// shape — exact variants fixed by a later pass.
/// See docs/agent/world/ecs-runtime.md (RequestHumanAction).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HumanAction {
    /// Ask the human to provide a text response to a prompt.
    Prompt { text: String },
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
    fn autonomy_variants_round_trip() {
        round_trip(&Autonomy::AskEverything);
        round_trip(&Autonomy::GateTier(Tier::FreeRun));
        round_trip(&Autonomy::GateTier(Tier::FlagReversible));
        round_trip(&Autonomy::GateTier(Tier::BlockIrreversible));
        round_trip(&Autonomy::RunFree);
    }

    #[test]
    fn agent_interaction_round_trips() {
        round_trip(&AgentInteraction {
            payload: serde_json::json!({ "kind": "approval", "action": "delete-file" }),
        });
    }

    #[test]
    fn interaction_response_variants_round_trip() {
        round_trip(&InteractionResponse::Accepted);
        round_trip(&InteractionResponse::Rejected);
        round_trip(&InteractionResponse::Data(serde_json::json!("yes")));
    }

    #[test]
    fn notify_variants_round_trip() {
        round_trip(&Notify::Push);
        round_trip(&Notify::Silent);
    }

    #[test]
    fn human_action_prompt_round_trips() {
        round_trip(&HumanAction::Prompt {
            text: "Please confirm the file list.".into(),
        });
    }
}
