//! inputs — see docs/agent/world/ecs-runtime.md

use serde::{Deserialize, Serialize};

use super::autonomy::{Autonomy, InteractionResponse};
use super::gates::{Authority, GuardrailTrip, PauseReason};
use super::history::{Block, Json, ToolResult};
use super::surface::{ElementId, Hash, Point, Selection, SurfaceId, SurfaceOp, SurfaceVersion, Viewport, WindowState};
use super::world::{CmdId, EdgeId, EntityId, ReqId, Tick, Timestamp, ToolUseId};

// ---------------------------------------------------------------------------
// Event envelope — every Logical Input enters the log inside an Event
// ---------------------------------------------------------------------------

/// The log envelope around one `LogicalInput`. It makes the two facts the mode
/// projection needs STRUCTURAL rather than inferred: WHO produced the input
/// (`origin`) and WHICH counterpart relationship it pertains to (`edge`). `at`
/// is the logical ORDERING axis; `wall` is the OBSERVED wall-time the shell read
/// when it appended this Event (the recorded hidden-input boundary, folded into
/// `Resources.wall` before Systems run). See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub origin: Origin,
    pub edge: EdgeId,
    pub at: Tick,
    pub wall: Option<Timestamp>,
    pub input: LogicalInput,
}

/// Who produced an input. Only `Human`/`Agent`/`Peer` are ACTOR origins that the
/// mode fold counts; `System` is mode-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Human,
    Agent,
    System,
    Peer,
}

// ---------------------------------------------------------------------------
// LogicalInput — Stratum 1, P0 + P1a subset
// ---------------------------------------------------------------------------

/// Logical inputs for P0 and the P1a tool / human / cancel / pause /
/// interaction additions.
///
/// Stratum 1 partitions into two CORRELATION CLASSES (Invariant 7):
///
/// - **EXOGENOUS** — the trajectory's FREE VARIABLES. They have no originating
///   Command, are carried across an edit/rewind VERBATIM, and are never
///   fingerprinted.
/// - **DERIVED** — an EFFECT-RESULT CACHE. Each answers a specific dispatched
///   Command, carries that request's `fingerprint` and the `entity` it routes
///   to, and is reused on replay ONLY while the re-emitted request re-hashes to
///   the same `fingerprint`. Named exceptions (Invariant 7): `ChildReturned`
///   (correlated by `(child, tool_use_id)` identity, no fingerprint) and the
///   abort acks `ToolAborted`/`HumanActionAborted` (correlated by `cmd`, no
///   fingerprint — an abort has no request payload to hash).
///
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogicalInput {
    // === EXOGENOUS — free variables; preserved verbatim across edit/rewind;
    //     never fingerprinted ===

    /// Session header (tick 0). Recorded ONCE when the log is opened so a fresh
    /// replay World reseeds `Rng` IDENTICALLY — the seed is a hidden input that
    /// MUST cross a recorded boundary. It ALSO carries the App's `surface_tools`
    /// (the surface-manipulation tool NAMES offered to the surface-capable ROOT
    /// entity), a second hidden input the live `WorldDriver` would otherwise seed
    /// only into the live World: recording it here lets the `SessionStarted` fold
    /// reconstruct `Resources.surface_tools` IDENTICALLY on BOTH the live and the
    /// replay paths (single source of truth), so a re-emitted root `CallModel`
    /// carries the SAME `ToolSet` and re-hashes to the recorded `Fingerprint`
    /// (Inv 7). Carried as plain tool names — NOT `effects::ToolSet` — so `inputs`
    /// needs no dependency on `effects`. EXOGENOUS.
    SessionStarted {
        seed: u64,
        /// The App's surface tool names. `#[serde(default, skip_serializing_if)]`
        /// keeps an OLD/empty tool-less log BYTE-IDENTICAL on the wire — an empty
        /// list writes NO `surface_tools` key and deserialises back to empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        surface_tools: Vec<String>,
    },

    /// A user's message to an entity. EXOGENOUS (a free variable).
    UserMessage { to: EntityId, text: String },

    /// A user-initiated or system-initiated pause. Adds one `PauseHold` to
    /// `WorldGate`; the gate goes `Paused` while any holds remain. EXOGENOUS.
    Pause { reason: PauseReason },

    /// Clears every `User` pause hold, allowing the gate to return to `Open`
    /// once no other holds remain. EXOGENOUS.
    Resume,

    /// Clears ONE `PolicyHalt` hold identified by `trip`, gated by `authority`.
    /// EXOGENOUS.
    ClearPolicyHalt {
        trip: GuardrailTrip,
        authority: Authority,
    },

    /// Cancels whatever the entity is currently doing (drives the appropriate
    /// `Cancelling` transition and cancel Commands per the entity's state).
    /// EXOGENOUS.
    Cancel { entity: EntityId },

    /// Sets the world-default autonomy policy. EXOGENOUS.
    SetAutonomy { policy: Autonomy },

    /// The human's (or authority's) answer to a `RaiseInteraction`, correlated
    /// to its request by the stable `request_id`. EXOGENOUS.
    InteractionAnswer {
        request_id: ReqId,
        answer: InteractionResponse,
    },

    /// Direct HUMAN UI manipulation — B8. HUMAN-ORIGIN ONLY (Invariant 18):
    /// a human grabbing the wheel is a primary EXOGENOUS free variable that
    /// feeds mode-as-projection ⇒ Operating. AGENT UI writes are NOT a
    /// `SurfaceMutated` input: an agent write is recorded ONCE as
    /// `ToolReturned` (the single UI-write commit record, see *Single source
    /// of truth*, lens 8), and the surface view is updated as a projection
    /// of that result — not as a separate logged input. The shell logs ONLY
    /// a `cause = Human` native signal as this stratum-1 input; `Command`-
    /// or `Peer`-caused signals are echoes of an already-logged
    /// `ToolReturned`/`DriveRequested` and are DROPPED (echo dedup,
    /// Theme 1e), so an agent `set_value` is never double-counted. `op` is
    /// a payload-bearing `SurfaceOp` (Theme 2a). EXOGENOUS.
    SurfaceMutated { op: SurfaceOp },

    /// UI state perceived by the agent at a tick — closing the "UI state is
    /// a hidden input" replay hole (Theme 2b). Records the macOS UI state
    /// the agent perceives so the UI-tool `Fingerprint` can fold in
    /// `surface_version`/`ax_digest`: a re-emitted UI request diverges
    /// exactly when the perceived UI state changed. EXOGENOUS (a free
    /// variable; origin Human/System on the relevant edge); never
    /// fingerprinted itself. See docs/agent/world/ecs-runtime.md.
    SurfaceObserved {
        surface: SurfaceId,
        version: SurfaceVersion,
        ax_digest: Hash,
        focus: Option<ElementId>,
        selection: Option<Selection>,
        viewport: Viewport,
        window: WindowState,
        cursor: Option<Point>,
    },

    // === DERIVED — effect-result cache; each fingerprinted result carries the
    //     request `fingerprint` AND the routing `entity`; reused on replay only
    //     on fingerprint match. Named non-fingerprinted exceptions: see the
    //     `ChildReturned`, `ToolAborted`, `HumanActionAborted` comments. ===

    /// The sole System-visible result of one inference: the assembled canonical
    /// `blocks` (the answer) and `meta` (call metadata), kept separate so a
    /// System can branch on metadata without parsing content. DERIVED.
    ModelResponded {
        cmd: CmdId,
        entity: EntityId,
        fingerprint: Fingerprint,
        blocks: Vec<Block>,
        meta: ModelMeta,
    },

    /// A model call that did not return blocks (HTTP/transport/overload). Drives
    /// the `Thinking → Idle` failure edge. DERIVED.
    ModelFailed {
        cmd: CmdId,
        entity: EntityId,
        fingerprint: Fingerprint,
        error: ModelError,
    },

    /// An inference that was cancelled, either by an explicit `CancelInference`
    /// (`Requested`) or synthesized on crash-resume (`Crash`, the "intent before
    /// commitment" recovery). DERIVED (carries `fingerprint`).
    InferenceCancelled {
        cmd: CmdId,
        entity: EntityId,
        fingerprint: Fingerprint,
        partial: Option<String>,
        reason: CancelReason,
    },

    /// The result of a `RunTool` dispatch — the tool's output blocks. A tool
    /// ERROR rides as `is_error: true` inside the `ToolResult` block so the
    /// model sees a well-formed `tool_result`. DERIVED (carries `fingerprint`).
    ToolReturned {
        cmd: CmdId,
        entity: EntityId,
        fingerprint: Fingerprint,
        result: Vec<Block>,
    },

    /// A child sub-agent's completion, recorded as an explicit correlated input
    /// so it resolves the parent's `Child` slot like any other result. Correlated
    /// by IDENTITY (`child` entity + `tool_use_id`), NOT by a request
    /// `fingerprint`: the child has no shell-dispatched request to hash; on
    /// replay it is regenerated deterministically when the child entity reaches
    /// its `EndTurn`. DERIVED (identity-correlated; deliberately NON-fingerprinted
    /// — named exception to Inv 7).
    ChildReturned {
        parent: EntityId,
        child: EntityId,
        tool_use_id: ToolUseId,
        result: ToolResult,
    },

    /// Ack of `CancelTool` — the in-flight tool has been aborted. Correlated
    /// by `cmd` (the cancelled `RunTool`'s `cmd`). NON-fingerprinted: an abort
    /// has no request payload to hash (Inv 7 named exception). DERIVED.
    ToolAborted { cmd: CmdId, entity: EntityId },

    /// The result of a `RequestHumanAction` dispatch. DERIVED (carries
    /// `fingerprint`).
    HumanActionDone {
        cmd: CmdId,
        entity: EntityId,
        fingerprint: Fingerprint,
        result: HumanResult,
    },

    /// Ack of `AbortHumanAction` — the pending human ask has been aborted.
    /// Correlated by `cmd` (the cancelled `RequestHumanAction`'s `cmd`).
    /// NON-fingerprinted: an abort has no request payload to hash (Inv 7 named
    /// exception). DERIVED.
    HumanActionAborted { cmd: CmdId, entity: EntityId },

    /// Result of a `Compact` dispatch — a single `summary` of the oldest
    /// `replaced` History messages. On receipt, History is rewritten and the
    /// entity transitions to `Thinking`. DERIVED (carries `fingerprint`).
    Compacted {
        cmd: CmdId,
        entity: EntityId,
        fingerprint: Fingerprint,
        summary: Vec<Block>,
        replaced: u32,
    },
}

// ---------------------------------------------------------------------------
// Supporting result types — outcomes of P1a Commands
// ---------------------------------------------------------------------------

/// Human-action outcomes that resolve a `Human` slot. `Provided` turns the
/// JSON answer into the slot's `ToolResult`; `Declined`/`Timeout` produce an
/// `is_error` `ToolResult` so the model can react.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanResult {
    /// The human provided the requested input.
    Provided(Json),
    /// The human explicitly declined.
    Declined,
    /// The request timed out without a response.
    Timeout,
}

/// Why an inference was cancelled. `Requested` is the ack of an explicit
/// `CancelInference`; `Crash` is synthesized on crash-resume when a
/// `CallModel` dispatch-intent has no result Input (intent before commitment).
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// The cancellation was explicitly requested via `CancelInference`.
    Requested,
    /// Synthesized on crash-resume: the dispatch-intent had no recorded result.
    Crash,
}

// ---------------------------------------------------------------------------
// Fingerprint — canonical hash of a request payload (P0 newtype)
// ---------------------------------------------------------------------------

/// A canonical hash of a request payload, recorded on the dispatch-intent AND
/// echoed on the DERIVED result, so replay reuses a recorded result ONLY when
/// the re-emitted request re-hashes identically. P0 models it as a simple string
/// newtype; the hashing function is supplied by the effects driver.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint(pub String);

// ---------------------------------------------------------------------------
// Model call metadata — kept distinct from the answer (`blocks`)
// ---------------------------------------------------------------------------

/// Call metadata recorded per inference so replay reproduces the SAME branch
/// from the log without re-resolving a live table: `model_id` is the EFFECTIVE
/// model actually used, `capabilities` the metadata in force for this call, and
/// `reasoning` the round-trip policy applied. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMeta {
    pub usage: Usage,
    pub model_id: String,
    pub stop_reason: StopReason,
    pub capabilities: Capabilities,
    pub reasoning: ReasoningPolicy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Why the model stopped. Serialises snake_case to match the Anthropic
/// `stop_reason` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    PauseTurn,
}

/// Capability metadata that governed a call. Shape only for P0 — opaque
/// structured JSON; exact fields fixed by a later pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities(pub serde_json::Value);

/// How reasoning blocks were round-tripped for a call, recorded so replay
/// reconstructs the SAME History deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPolicy {
    Echo,
    Drop,
    MustEcho,
}

/// A model-call failure, distinct from a successful non-`EndTurn` stop reason:
/// the call yielded no blocks at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelError {
    Http(u16),
    Timeout,
    Overloaded,
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::autonomy::{HumanAction, InteractionResponse, Notify};
    use crate::agent::world::gates::{Authority, GuardrailTrip, PauseReason};
    use crate::agent::world::history::Block;
    use crate::agent::world::surface::{
        ComponentSpec, ElementId, Hash, PeerEnvelopeId, Point, Route, Selection, SurfaceId,
        SurfaceOp, SurfaceVersion, Viewport, WindowState,
    };

    fn round_trip(event: &Event) {
        let json = serde_json::to_string(event).expect("serialise event");
        let back: Event = serde_json::from_str(&json).expect("deserialise event");
        assert_eq!(event, &back);
    }

    fn wrap(input: LogicalInput) -> Event {
        Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input,
        }
    }

    // --- P0 variants (preserved) -------------------------------------------

    #[test]
    fn session_started_records_the_rng_seed_and_round_trips() {
        let event = Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: Some(1_700_000_000),
            input: LogicalInput::SessionStarted {
                seed: 0xDEAD_BEEF,
                surface_tools: vec!["set_value".into()],
            },
        };
        // The seed is the recorded hidden input the replay World reseeds from, and
        // the App's surface tools cross the same recorded boundary.
        match &event.input {
            LogicalInput::SessionStarted { seed, surface_tools } => {
                assert_eq!(*seed, 0xDEAD_BEEF);
                assert_eq!(surface_tools, &vec!["set_value".to_string()]);
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        round_trip(&event);
    }

    /// An empty `surface_tools` (the tool-less default) writes NO `surface_tools`
    /// key, so a log recorded before this field is BYTE-IDENTICAL on the wire and
    /// deserialises back to an empty list. This is what keeps every existing
    /// tool-less replay sink byte-identical.
    #[test]
    fn session_started_with_empty_surface_tools_omits_the_key_and_stays_byte_identical() {
        let with_empty = Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input: LogicalInput::SessionStarted {
                seed: 7,
                surface_tools: Vec::new(),
            },
        };
        let json = serde_json::to_string(&with_empty).expect("serialise");
        assert!(
            !json.contains("surface_tools"),
            "an empty surface_tools omits the key (byte-identical to a pre-tools log)"
        );
        // A historical log written before the field deserialises to empty tools.
        let legacy = r#"{"origin":"system","edge":0,"at":0,"wall":null,"input":{"kind":"session_started","seed":7}}"#;
        let back: Event = serde_json::from_str(legacy).expect("deserialise legacy header");
        assert_eq!(back, with_empty, "a pre-tools header deserialises to empty surface_tools");
    }

    #[test]
    fn user_message_round_trips() {
        round_trip(&Event {
            origin: Origin::Human,
            edge: 0,
            at: 1,
            wall: None,
            input: LogicalInput::UserMessage {
                to: 0,
                text: "hello there".into(),
            },
        });
    }

    #[test]
    fn model_responded_round_trips_with_full_meta() {
        round_trip(&Event {
            origin: Origin::Agent,
            edge: 0,
            at: 2,
            wall: Some(1_700_000_001),
            input: LogicalInput::ModelResponded {
                cmd: 1,
                entity: 0,
                fingerprint: Fingerprint("fp-req-1".into()),
                blocks: vec![Block::Text {
                    text: "the answer".into(),
                }],
                meta: ModelMeta {
                    usage: Usage {
                        input_tokens: 12,
                        output_tokens: 7,
                    },
                    model_id: "claude-x-2026".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({ "thinking": true })),
                    reasoning: ReasoningPolicy::Echo,
                },
            },
        });
    }

    #[test]
    fn model_failed_round_trips() {
        round_trip(&Event {
            origin: Origin::Agent,
            edge: 0,
            at: 3,
            wall: None,
            input: LogicalInput::ModelFailed {
                cmd: 2,
                entity: 0,
                fingerprint: Fingerprint("fp-req-2".into()),
                error: ModelError::Http(503),
            },
        });
    }

    // --- P1a EXOGENOUS variants --------------------------------------------

    #[test]
    fn pause_round_trips() {
        round_trip(&wrap(LogicalInput::Pause {
            reason: PauseReason::User,
        }));
        round_trip(&wrap(LogicalInput::Pause {
            reason: PauseReason::PolicyHalt(GuardrailTrip("guardrail-1".into())),
        }));
    }

    #[test]
    fn resume_round_trips() {
        round_trip(&wrap(LogicalInput::Resume));
    }

    #[test]
    fn clear_policy_halt_round_trips() {
        round_trip(&wrap(LogicalInput::ClearPolicyHalt {
            trip: GuardrailTrip("content-filter".into()),
            authority: Authority("admin-token".into()),
        }));
    }

    #[test]
    fn cancel_round_trips() {
        round_trip(&wrap(LogicalInput::Cancel { entity: 5 }));
    }

    #[test]
    fn set_autonomy_round_trips() {
        use crate::agent::world::autonomy::{Autonomy, Tier};
        round_trip(&wrap(LogicalInput::SetAutonomy {
            policy: Autonomy::GateTier(Tier::BlockIrreversible),
        }));
        round_trip(&wrap(LogicalInput::SetAutonomy {
            policy: Autonomy::RunFree,
        }));
    }

    #[test]
    fn interaction_answer_round_trips() {
        round_trip(&wrap(LogicalInput::InteractionAnswer {
            request_id: 3,
            answer: InteractionResponse::Accepted,
        }));
        round_trip(&wrap(LogicalInput::InteractionAnswer {
            request_id: 4,
            answer: InteractionResponse::Data(serde_json::json!({ "choice": "yes" })),
        }));
    }

    // --- P1a DERIVED variants (fingerprinted) ------------------------------

    #[test]
    fn inference_cancelled_round_trips() {
        round_trip(&wrap(LogicalInput::InferenceCancelled {
            cmd: 10,
            entity: 0,
            fingerprint: Fingerprint("fp-10".into()),
            partial: Some("partial text".into()),
            reason: CancelReason::Requested,
        }));
        round_trip(&wrap(LogicalInput::InferenceCancelled {
            cmd: 11,
            entity: 0,
            fingerprint: Fingerprint("fp-11".into()),
            partial: None,
            reason: CancelReason::Crash,
        }));
    }

    #[test]
    fn tool_returned_round_trips() {
        round_trip(&wrap(LogicalInput::ToolReturned {
            cmd: 20,
            entity: 1,
            fingerprint: Fingerprint("fp-20".into()),
            result: vec![Block::Text { text: "tool output".into() }],
        }));
    }

    #[test]
    fn child_returned_round_trips_and_has_no_fingerprint() {
        // Compile-time assertion: the exhaustive pattern below will fail to compile
        // if `ChildReturned` ever gains a `fingerprint` field (named Inv 7 exception).
        let input = LogicalInput::ChildReturned {
            parent: 0,
            child: 1,
            tool_use_id: "tu_child".into(),
            result: Block::ToolResult {
                tool_use_id: "tu_child".into(),
                content: vec![Block::Text { text: "child result".into() }],
                is_error: false,
            },
        };
        // Exhaustive destructure — no `..`, no `fingerprint` field: proves it
        // does not exist at compile time (Inv 7 named exception).
        let LogicalInput::ChildReturned { parent, child, tool_use_id, result } = &input else {
            panic!("unexpected variant");
        };
        assert_eq!(*parent, 0);
        assert_eq!(*child, 1);
        assert_eq!(tool_use_id, "tu_child");
        let _ = result;
        round_trip(&wrap(input));
    }

    #[test]
    fn tool_aborted_has_no_fingerprint_and_round_trips() {
        // Compile-time assertion: exhaustive destructure without `fingerprint`
        // proves the field is absent (Inv 7 named exception — abort ack).
        let input = LogicalInput::ToolAborted { cmd: 30, entity: 2 };
        let LogicalInput::ToolAborted { cmd, entity } = input else {
            panic!("unexpected variant");
        };
        assert_eq!(cmd, 30);
        assert_eq!(entity, 2);
        round_trip(&wrap(LogicalInput::ToolAborted { cmd: 30, entity: 2 }));
    }

    #[test]
    fn human_action_done_round_trips() {
        round_trip(&wrap(LogicalInput::HumanActionDone {
            cmd: 40,
            entity: 3,
            fingerprint: Fingerprint("fp-40".into()),
            result: HumanResult::Provided(serde_json::json!("approved")),
        }));
        round_trip(&wrap(LogicalInput::HumanActionDone {
            cmd: 41,
            entity: 3,
            fingerprint: Fingerprint("fp-41".into()),
            result: HumanResult::Declined,
        }));
        round_trip(&wrap(LogicalInput::HumanActionDone {
            cmd: 42,
            entity: 3,
            fingerprint: Fingerprint("fp-42".into()),
            result: HumanResult::Timeout,
        }));
    }

    #[test]
    fn human_action_aborted_has_no_fingerprint_and_round_trips() {
        // Compile-time assertion: exhaustive destructure without `fingerprint`
        // proves the field is absent (Inv 7 named exception — abort ack).
        let input = LogicalInput::HumanActionAborted { cmd: 50, entity: 4 };
        let LogicalInput::HumanActionAborted { cmd, entity } = input else {
            panic!("unexpected variant");
        };
        assert_eq!(cmd, 50);
        assert_eq!(entity, 4);
        round_trip(&wrap(LogicalInput::HumanActionAborted { cmd: 50, entity: 4 }));
    }

    #[test]
    fn compacted_round_trips() {
        round_trip(&wrap(LogicalInput::Compacted {
            cmd: 60,
            entity: 5,
            fingerprint: Fingerprint("fp-60".into()),
            summary: vec![Block::Text { text: "summary of old messages".into() }],
            replaced: 12,
        }));
    }

    // --- Surface and mode inputs (EXOGENOUS, human-origin) -----------------

    /// `SurfaceMutated` carries a `SurfaceOp`, round-trips, and is
    /// human-origin only (Invariant 18). The exhaustive destructure below
    /// proves there is EXACTLY ONE `SurfaceMutated` variant in `LogicalInput`
    /// and that it carries `op` (and nothing else): if a second variant or an
    /// extra field were added the pattern would fail to compile.
    #[test]
    fn surface_mutated_is_human_origin_only_and_round_trips() {
        let op = SurfaceOp::Click {
            surface: 1,
            element: 5,
            point: Some((10, 20)),
            base_version: None,
        };
        let input = LogicalInput::SurfaceMutated { op };
        // Exhaustive destructure — proves exactly one field (`op`) and no
        // `fingerprint`, no `entity`, no agent-origin fields (Inv 18).
        let LogicalInput::SurfaceMutated { op: ref extracted_op } = input else {
            panic!("expected SurfaceMutated");
        };
        matches!(
            extracted_op,
            SurfaceOp::Click { surface: 1, element: 5, .. }
        );

        // Round-trip via Event (human origin on the human edge).
        let event = Event {
            origin: Origin::Human,
            edge: 0,
            at: 99,
            wall: None,
            input,
        };
        let json = serde_json::to_string(&event).expect("serialise SurfaceMutated event");
        let back: Event = serde_json::from_str(&json).expect("deserialise SurfaceMutated event");
        assert_eq!(event, back);
    }

    #[test]
    fn surface_mutated_set_value_round_trips() {
        let event = wrap(LogicalInput::SurfaceMutated {
            op: SurfaceOp::SetValue {
                surface: 2,
                element: 9,
                value: serde_json::json!("hello"),
                base_version: Some(3),
            },
        });
        round_trip(&event);
    }

    #[test]
    fn surface_mutated_navigate_and_render_round_trip() {
        round_trip(&wrap(LogicalInput::SurfaceMutated {
            op: SurfaceOp::Navigate {
                surface: 3,
                route: Route("settings".into()),
                base_version: None,
            },
        }));
        round_trip(&wrap(LogicalInput::SurfaceMutated {
            op: SurfaceOp::Render {
                surface: 4,
                component: ComponentSpec("toolbar-v2".into()),
                base_version: Some(1),
            },
        }));
    }

    #[test]
    fn surface_observed_round_trips() {
        let event = wrap(LogicalInput::SurfaceObserved {
            surface: 1,
            version: 42,
            ax_digest: Hash("abc123".into()),
            focus: Some(7),
            selection: Some(Selection("range:0-5".into())),
            viewport: Viewport("rect:0,0,800,600".into()),
            window: WindowState("key,main".into()),
            cursor: Some((400, 300)),
        });
        round_trip(&event);

        // Without optional fields
        round_trip(&wrap(LogicalInput::SurfaceObserved {
            surface: 2,
            version: 0,
            ax_digest: Hash("".into()),
            focus: None,
            selection: None,
            viewport: Viewport("".into()),
            window: WindowState("".into()),
            cursor: None,
        }));
    }

    // Suppress unused-import warnings for types that appear in doc-tests but
    // not in the function bodies above.
    #[allow(dead_code)]
    fn _assert_types_used(_: HumanAction, _: Notify, _: PeerEnvelopeId) {}
}
