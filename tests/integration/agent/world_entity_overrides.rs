//! VC-3.1 — per-entity model/autonomy overrides resolved OFFLINE through the REAL
//! resolution path: the actual `CallModel` EMIT carries the resolved model, and the
//! autonomy GATE reads the resolved autonomy. A `Some` override resolves to the
//! entity's own value; a `None` override INHERITS the world default
//! (`Resources.model` / `Resources.autonomy`). (US-4; design §343-348.)
//!
//! This is the deterministic, NON-VACUOUS half of US-4 — it touches no
//! `src/agent/world/*` file and makes no live model call, so it runs on every
//! machine. Non-vacuity is load-bearing here: a `None`-only test would pass against
//! the world default no matter what `model_for`/`autonomy_for` did, so each case
//! PINS the `Some` override DISTINCTLY from the inherited default —
//!
//! - **model** — the override entity's emitted `CallModel.params` is the override
//!   `ModelConfig`, which is asserted DISTINCT from the world default the `None`
//!   entity's `CallModel` carries. Were the override ignored, the override entity's
//!   call would carry the world default and the equality would fail.
//! - **autonomy** — under a world default of `RunFree` (which does NOT gate), the
//!   override entity (`Some(AskEverything)`) GATES its `Local` tool slot
//!   (`RaiseInteraction`, no `RunTool`) while the `None` entity runs it free
//!   (`RunTool`, no `RaiseInteraction`). The gate flips ONLY because the override is
//!   consulted; were it ignored, the override entity would run free like the default.
//!
//! Both cases drive the pure `tick` reducer (the same reducer the live driver folds
//! through), so the resolution is exercised at its real emit/gate site — not via a
//! direct `World::model_for`/`autonomy_for` unit call (those are covered by the
//! `world.rs` unit tests). See docs/agent/world/ecs-runtime.md §343-348 and the
//! plan's Verification §3 VC-3.1.

use rubberdux::agent::world::autonomy::Autonomy;
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::Command;
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources,
    Tick, World,
};

const SEED: u64 = 7;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An `Idle` entity carrying the given per-entity overrides (`None` ⇒ inherit). The
/// `identity`/`lineage` let a sub-agent (US-4: "a sub-agent runs a cheaper model")
/// be modelled alongside the primary in the SAME World.
fn entity(
    identity: Identity,
    parent: Option<EntityId>,
    depth: u8,
    model: Option<ModelConfig>,
    autonomy: Option<Autonomy>,
) -> Components {
    Components {
        identity,
        lineage: Lineage { parent, depth },
        history: History::default(),
        activity: Activity::Idle,
        gate: EntityGate::default(),
        budget: Budget::default(),
        inbox: Inbox::default(),
        turns: 0,
        spawned: 0,
        model,
        autonomy,
    }
}

fn user_message(to: EntityId, text: &str, at: Tick) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::UserMessage {
            to,
            text: text.into(),
        },
    }
}

/// A `ModelResponded·ToolUse` carrying one NON-spawn, NON-peer tool block — a
/// `Local` tool slot, which AutonomySystem gates under a gating policy — routed to
/// `entity` on its turn `cmd`.
fn model_responded_local_tool(cmd: CmdId, entity: EntityId, tool_use_id: &str, at: Tick) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity,
            fingerprint: Fingerprint("fp".into()),
            blocks: vec![Block::ToolUse {
                id: tool_use_id.into(),
                name: "do_thing".into(),
                input: serde_json::json!({ "arg": 1 }),
            }],
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

/// The `ModelConfig` (the override `params`) of the FIRST `CallModel` emitted this
/// tick — the value the live driver feeds `MessageBuilder` as `request_body["model"]`.
fn call_model_params(commands: &[Command]) -> ModelConfig {
    commands
        .iter()
        .find_map(|c| match c {
            Command::CallModel { params, .. } => Some(params.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the turn must emit a CallModel; got {commands:?}"))
}

/// The `cmd` of the FIRST `CallModel` emitted this tick, so the following
/// `ModelResponded` can correlate to the turn it opened.
fn call_model_cmd(commands: &[Command]) -> CmdId {
    commands
        .iter()
        .find_map(|c| match c {
            Command::CallModel { cmd, .. } => Some(*cmd),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the turn must emit a CallModel; got {commands:?}"))
}

// ---------------------------------------------------------------------------
// VC-3.1 — per-entity MODEL override resolved at the real CallModel emit
// ---------------------------------------------------------------------------

/// **VC-3.1** — driving a turn for an entity with `Components.model = Some(m)` emits a
/// `CallModel` whose `params` is `m`; an entity with `model = None` emits a `CallModel`
/// carrying the world default (`Resources.model`). Both go through the REAL emit path
/// (IntakeSystem → `World::model_for`), and the `Some` case is asserted DISTINCT from
/// the inherited default — so the override is pinned, not merely consistent with the
/// default (non-vacuous).
#[test]
fn vc_3_1_model_override_resolves_at_call_model_emit_else_inherits_world_default() {
    let world_default = ModelConfig {
        model: "world-default-model".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    };
    let override_model = ModelConfig {
        model: "entity-override-model".into(),
        max_tokens: 256,
        effort: Effort::Low,
    };
    // Non-vacuity precondition: the override is OBSERVABLY distinct from the default,
    // so "resolves to the override" cannot be satisfied by the inherited default.
    assert_ne!(
        override_model, world_default,
        "the override ModelConfig must differ from the world default for a non-vacuous proof"
    );

    let mut world = World::new(0, Resources::new(SEED, world_default.clone()));
    // Entity 0 (primary/root): `model: None` ⇒ inherits the world default.
    world
        .entities
        .insert(0, entity(Identity::Primary, None, 0, None, None));
    // Entity 1 (a sub-agent): `model: Some(override)` ⇒ resolves to its own model.
    world.entities.insert(
        1,
        entity(
            Identity::Subagent,
            Some(0),
            1,
            Some(override_model.clone()),
            None,
        ),
    );

    // The `None` entity's turn: the emitted CallModel carries the WORLD DEFAULT.
    let (_after_root, root_cmds) = tick(&world, &user_message(0, "go root", 1));
    let root_params = call_model_params(&root_cmds);
    assert_eq!(
        root_params, world_default,
        "model: None inherits Resources.model at the real CallModel emit"
    );

    // The `Some` entity's turn: the emitted CallModel carries the OVERRIDE.
    let (_after_child, child_cmds) = tick(&world, &user_message(1, "go child", 2));
    let child_params = call_model_params(&child_cmds);
    assert_eq!(
        child_params, override_model,
        "model: Some(override) resolves to the entity's own model at the real CallModel emit"
    );

    // Non-vacuity: the override the sub-agent emitted is DISTINCT from the default the
    // primary inherited — the `Some` is pinned, not coincidentally equal to the default.
    assert_ne!(
        child_params, root_params,
        "the Some(override) CallModel is pinned DISTINCTLY from the None/default CallModel"
    );
}

// ---------------------------------------------------------------------------
// VC-3.1 — per-entity AUTONOMY override resolved at the real autonomy gate
// ---------------------------------------------------------------------------

/// **VC-3.1** — under a world default of `RunFree` (which never gates), an entity with
/// `Components.autonomy = Some(AskEverything)` GATES its `Local` tool slot at the real
/// autonomy gate (AutonomySystem → `World::autonomy_for`): it emits `RaiseInteraction`
/// and withholds `RunTool`. An entity with `autonomy = None` INHERITS the `RunFree`
/// default and runs the slot free (`RunTool`, no `RaiseInteraction`). The gate flips
/// ONLY because the override is consulted — were it ignored, the override entity would
/// run free like the default — so the `Some` case is pinned distinctly (non-vacuous).
#[test]
fn vc_3_1_autonomy_override_resolves_at_the_gate_else_inherits_world_default() {
    let model = ModelConfig {
        model: "claude-x".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    };
    let mut world = World::new(0, Resources::new(SEED, model));
    // The world default policy is RunFree — `Resources::new` seeds it, and RunFree
    // NEVER gates, so any gating below can only come from a per-entity override.
    assert_eq!(
        world.resources.autonomy,
        Autonomy::RunFree,
        "the world default autonomy is RunFree (never gates)"
    );

    // Entity 0 (primary/root): `autonomy: None` ⇒ inherits the world default (RunFree).
    world
        .entities
        .insert(0, entity(Identity::Primary, None, 0, None, None));
    // Entity 1 (a sub-agent): `autonomy: Some(AskEverything)` ⇒ resolves to its own
    // (gating) policy, DISTINCT from the world default.
    world.entities.insert(
        1,
        entity(
            Identity::Subagent,
            Some(0),
            1,
            None,
            Some(Autonomy::AskEverything),
        ),
    );

    // --- The `None` entity: inherits RunFree ⇒ the Local slot runs FREE -------------
    let (root_thinking, root_turn) = tick(&world, &user_message(0, "go", 1));
    let root_cmd = call_model_cmd(&root_turn);
    let (_root_resolved, root_after) = tick(
        &root_thinking,
        &model_responded_local_tool(root_cmd, 0, "tu_root", 2),
    );
    assert!(
        root_after
            .iter()
            .any(|c| matches!(c, Command::RunTool { .. })),
        "autonomy: None inherits RunFree ⇒ the Local slot dispatches RunTool directly"
    );
    assert!(
        !root_after
            .iter()
            .any(|c| matches!(c, Command::RaiseInteraction { .. })),
        "autonomy: None inherits RunFree ⇒ NO approval interaction is raised"
    );

    // --- The `Some` entity: AskEverything ⇒ the Local slot is GATED -----------------
    let (child_thinking, child_turn) = tick(&world, &user_message(1, "go", 3));
    let child_cmd = call_model_cmd(&child_turn);
    let (_child_resolved, child_after) = tick(
        &child_thinking,
        &model_responded_local_tool(child_cmd, 1, "tu_child", 4),
    );
    assert!(
        child_after
            .iter()
            .any(|c| matches!(c, Command::RaiseInteraction { .. })),
        "autonomy: Some(AskEverything) resolves at the gate ⇒ RaiseInteraction is emitted"
    );
    assert!(
        !child_after
            .iter()
            .any(|c| matches!(c, Command::RunTool { .. })),
        "autonomy: Some(AskEverything) resolves at the gate ⇒ RunTool is WITHHELD pending approval"
    );

    // Non-vacuity: the world default is RunFree, which the root demonstrably ran free
    // under; the sub-agent gated ONLY because its per-entity override was consulted.
    // Were `autonomy_for` to ignore the override, the sub-agent would have run free too
    // (RunTool, no RaiseInteraction) and the assertions above would fail.
}
