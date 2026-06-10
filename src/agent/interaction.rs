//! The unified, fixed vocabulary an agent uses to raise a human-facing
//! interaction. See `docs/agent/interaction.md` for the conceptual model and
//! the whiteboard specification decision (D2) that fixes this vocabulary.
//!
//! The types here are the single vocabulary the whole system speaks. The
//! pre-existing external-agent types (`UIInteractionRequest` /
//! `UIInteractionResponse` in `crate::agent::external`) bridge to and from
//! this vocabulary losslessly via `From` conversions defined alongside them,
//! so Claude Code / Codex behavior is unchanged.

use serde::{Deserialize, Serialize};

/// The flavor of an [`AgentInteraction::Approval`]. Distinguishes a
/// general-action permission grant from a plan sign-off; both are a yes/no
/// decision but carry different intent for the observer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "approval", rename_all = "snake_case")]
pub enum ApprovalFlavor {
    /// Permission to perform a described action.
    Permission,
    /// Sign-off on a proposed plan.
    Plan,
}

/// One selectable option in an [`AgentInteraction::Question`] or
/// [`AgentInteraction::Choice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    /// Short label shown to the user.
    pub label: String,
    /// Longer explanation of what selecting this option means.
    pub description: String,
}

/// A generated artifact presented for acknowledgement by an
/// [`AgentInteraction::Preview`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewArtifact {
    /// Media type of the artifact body (e.g. `text/markdown`, `image/png`).
    pub mime_type: String,
    /// The artifact content. Encoding is determined by `mime_type`.
    pub content: String,
}

/// The fixed set of interaction primitives an agent may raise. Every variant
/// carries its `request_id` (correlates request and response) and `app_id`
/// (the app/agent task that raised it), exposed through accessors so call
/// sites need not match on the variant.
///
/// The `kind` tag is a stable serde discriminator, decoupled from the Rust
/// variant name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentInteraction {
    /// A yes/no decision on a described action or a plan.
    Approval {
        request_id: String,
        app_id: String,
        flavor: ApprovalFlavor,
        /// The action description or plan text to approve.
        prompt: String,
    },
    /// An open question, optionally with suggested options.
    Question {
        request_id: String,
        app_id: String,
        text: String,
        options: Vec<ChoiceOption>,
    },
    /// A forced selection among mutually exclusive options.
    Choice {
        request_id: String,
        app_id: String,
        prompt: String,
        options: Vec<ChoiceOption>,
    },
    /// A generated artifact presented for acknowledgement.
    Preview {
        request_id: String,
        app_id: String,
        prompt: String,
        artifact: PreviewArtifact,
    },
}

impl AgentInteraction {
    /// The identifier correlating this request with its [`InteractionResponse`].
    pub fn request_id(&self) -> &str {
        match self {
            AgentInteraction::Approval { request_id, .. }
            | AgentInteraction::Question { request_id, .. }
            | AgentInteraction::Choice { request_id, .. }
            | AgentInteraction::Preview { request_id, .. } => request_id,
        }
    }

    /// The app/agent task that raised this interaction.
    pub fn app_id(&self) -> &str {
        match self {
            AgentInteraction::Approval { app_id, .. }
            | AgentInteraction::Question { app_id, .. }
            | AgentInteraction::Choice { app_id, .. }
            | AgentInteraction::Preview { app_id, .. } => app_id,
        }
    }
}

/// The fixed set of replies to an [`AgentInteraction`]. Every variant carries
/// the `request_id` it answers. The `kind` tag is a stable serde
/// discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InteractionResponse {
    /// An [`AgentInteraction::Approval`] was granted. `flavor` records whether
    /// the granted approval was a permission or a plan sign-off so the legacy
    /// bridge can reconstruct the exact `UIInteractionResponse` variant
    /// (`PermissionGranted` vs `PlanApproved`) without information loss.
    Approved {
        request_id: String,
        flavor: ApprovalFlavor,
    },
    /// An [`AgentInteraction::Approval`] was declined, with a reason. `flavor`
    /// records whether the declined approval was a permission or a plan
    /// sign-off so the legacy bridge can reconstruct the exact
    /// `UIInteractionResponse` variant (`PermissionDenied` vs `PlanRejected`).
    Declined {
        request_id: String,
        flavor: ApprovalFlavor,
        reason: String,
    },
    /// A [`AgentInteraction::Question`] or [`AgentInteraction::Choice`] was
    /// answered. `selected` is the chosen option index when one was picked;
    /// `reply` is a free-form answer when no option fit.
    Answered {
        request_id: String,
        selected: Option<usize>,
        reply: Option<String>,
    },
    /// An [`AgentInteraction::Preview`] was acknowledged.
    Acknowledged { request_id: String },
}

impl InteractionResponse {
    /// The identifier of the [`AgentInteraction`] this response answers.
    pub fn request_id(&self) -> &str {
        match self {
            InteractionResponse::Approved { request_id, .. }
            | InteractionResponse::Declined { request_id, .. }
            | InteractionResponse::Answered { request_id, .. }
            | InteractionResponse::Acknowledged { request_id } => request_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn test_approval_round_trip_and_tag() {
        let interaction = AgentInteraction::Approval {
            request_id: "r1".into(),
            app_id: "app-1".into(),
            flavor: ApprovalFlavor::Permission,
            prompt: "delete file".into(),
        };
        let json = serde_json::to_value(&interaction).unwrap();
        assert_eq!(json["kind"], "approval");
        assert_eq!(round_trip(&interaction), interaction);
        assert_eq!(interaction.request_id(), "r1");
        assert_eq!(interaction.app_id(), "app-1");
    }

    #[test]
    fn test_question_round_trip_and_tag() {
        let interaction = AgentInteraction::Question {
            request_id: "r2".into(),
            app_id: "app-2".into(),
            text: "which db?".into(),
            options: vec![ChoiceOption {
                label: "pg".into(),
                description: "postgres".into(),
            }],
        };
        let json = serde_json::to_value(&interaction).unwrap();
        assert_eq!(json["kind"], "question");
        assert_eq!(round_trip(&interaction), interaction);
        assert_eq!(interaction.request_id(), "r2");
        assert_eq!(interaction.app_id(), "app-2");
    }

    #[test]
    fn test_choice_round_trip_and_tag() {
        let interaction = AgentInteraction::Choice {
            request_id: "r3".into(),
            app_id: "app-3".into(),
            prompt: "pick a theme".into(),
            options: vec![
                ChoiceOption {
                    label: "light".into(),
                    description: "light theme".into(),
                },
                ChoiceOption {
                    label: "dark".into(),
                    description: "dark theme".into(),
                },
            ],
        };
        let json = serde_json::to_value(&interaction).unwrap();
        assert_eq!(json["kind"], "choice");
        assert_eq!(round_trip(&interaction), interaction);
        assert_eq!(interaction.request_id(), "r3");
        assert_eq!(interaction.app_id(), "app-3");
    }

    #[test]
    fn test_preview_round_trip_and_tag() {
        let interaction = AgentInteraction::Preview {
            request_id: "r4".into(),
            app_id: "app-4".into(),
            prompt: "review this icon".into(),
            artifact: PreviewArtifact {
                mime_type: "image/png".into(),
                content: "base64data".into(),
            },
        };
        let json = serde_json::to_value(&interaction).unwrap();
        assert_eq!(json["kind"], "preview");
        assert_eq!(round_trip(&interaction), interaction);
        assert_eq!(interaction.request_id(), "r4");
        assert_eq!(interaction.app_id(), "app-4");
    }

    #[test]
    fn test_response_approved_round_trip_and_tag() {
        let response = InteractionResponse::Approved {
            request_id: "r1".into(),
            flavor: ApprovalFlavor::Permission,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["kind"], "approved");
        assert_eq!(round_trip(&response), response);
        assert_eq!(response.request_id(), "r1");
    }

    #[test]
    fn test_response_declined_round_trip_and_tag() {
        let response = InteractionResponse::Declined {
            request_id: "r1".into(),
            flavor: ApprovalFlavor::Plan,
            reason: "unsafe".into(),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["kind"], "declined");
        assert_eq!(round_trip(&response), response);
        assert_eq!(response.request_id(), "r1");
    }

    #[test]
    fn test_response_answered_round_trip_and_tag() {
        let response = InteractionResponse::Answered {
            request_id: "r2".into(),
            selected: Some(0),
            reply: None,
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["kind"], "answered");
        assert_eq!(round_trip(&response), response);
        assert_eq!(response.request_id(), "r2");
    }

    #[test]
    fn test_response_acknowledged_round_trip_and_tag() {
        let response = InteractionResponse::Acknowledged {
            request_id: "r4".into(),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["kind"], "acknowledged");
        assert_eq!(round_trip(&response), response);
        assert_eq!(response.request_id(), "r4");
    }
}
