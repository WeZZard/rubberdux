//! compaction — the CompactionSystem (phase 4 BudgetCompaction). See
//! docs/agent/world/ecs-runtime.md (CompactionSystem; Context-window compaction;
//! Invariants 10, 11).
//!
//! CompactionSystem is the context-window guard. It fires in phase 4 (after
//! BudgetSystem has folded the current call's usage into `Budget.context`) and
//! intercepts the next turn's `CallModel` when the entity's context occupancy
//! would overflow the `context_limit`. The continuation is DEFERRED behind a
//! `Compact` model call; on `Compacted` the summary is spliced into History and
//! the deferred continuation resumes; on a compaction `ModelFailed` the entity
//! proceeds UN-compacted (best-effort — a failed compaction is never a new sink,
//! Invariant 10).
//!
//! ## Trigger
//! On `ModelResponded`: if the entity is now `Idle` (TurnSystem phase 3 settled
//! it), context is over the limit (BudgetSystem phase 4 folded the usage), and
//! the entity's Inbox holds at least one pending message (a continuation IS
//! pending), CompactionSystem defers the continuation: `Idle → Compacting` +
//! `Command::Compact`. ToolSystem (phase 5) sees `Compacting` and emits nothing.
//!
//! ## Resume
//! `Compacted { cmd, summary, replaced }`: splice `summary` over the oldest
//! `replaced` History messages, drain the Inbox, mint a new `cmd`, transition
//! `Compacting → Thinking`, emit `CallModel` with the now-shorter History.
//!
//! ## Best-effort on failure
//! `ModelFailed { cmd }` (compaction failed): proceed without summary — drain
//! the Inbox into the un-compacted History, mint a new `cmd`, `→ Thinking` +
//! `CallModel`. The context may still be high, but the agent is never stuck.
//!
//! ## Hard overflow / halt
//! A hard context overflow with no inbox messages (no pending continuation) does
//! NOT compact. The entity remains `Idle`; any subsequent turn that sends a
//! `CallModel` will encounter the provider's context limit directly. An operator
//! may handle this via the existing `EntityHalt` / `ClearPolicyHalt` mechanism
//! (reused, not duplicated here — Invariant 15). Full auto-tuning is out of scope.

use super::{Input, System};
use crate::agent::world::effects::{Command, CommandKey, ToolSet};
use crate::agent::world::history::{Msg, Role};
use crate::agent::world::inputs::LogicalInput;
use crate::agent::world::world::{Activity, EntityId, World};

/// CompactionSystem — the context-window guard (phase 4 BudgetCompaction).
/// Pure reducer; see module-level docs and docs/agent/world/ecs-runtime.md.
pub struct CompactionSystem;

impl System for CompactionSystem {
    fn step(&self, world: &World, input: &Input) -> (World, Vec<Command>) {
        match input {
            // ----------------------------------------------------------------
            // ModelResponded: check for context pressure AFTER BudgetSystem has
            // folded the usage (we run later in phase 4). If the entity is now
            // Idle (TurnSystem settled it in phase 3), context is over the limit,
            // and the Inbox has a pending message (continuation pending), defer
            // the continuation behind a Compact. Gates guard NEW WORK (Inv 13).
            // ----------------------------------------------------------------
            LogicalInput::ModelResponded { entity, .. } => {
                let Some(e) = world.entities.get(entity) else {
                    return (world.clone(), Vec::new());
                };
                // Entity must be Idle (TurnSystem settled it from Thinking).
                if !matches!(e.activity, Activity::Idle) {
                    return (world.clone(), Vec::new());
                }
                // Context pressure: limit is set AND occupancy meets or exceeds it.
                if !e.budget.context_exceeded() {
                    return (world.clone(), Vec::new());
                }
                // Continuation must be pending: Inbox holds at least one message.
                if e.inbox.pending.is_empty() {
                    return (world.clone(), Vec::new());
                }
                // Gate guard (Inv 13): compaction is NEW WORK → both gates must be Open.
                if !world.resources.gate.is_open() || !e.gate.is_open() {
                    return (world.clone(), Vec::new());
                }

                // Defer the continuation: Idle → Compacting + Compact.
                let mut world = world.clone();
                let (cmd, ids) = world.resources.ids.mint_cmd();
                world.resources.ids = ids;
                let (upto, messages, params) = {
                    let Some(e) = world.entities.get(entity) else {
                        return (world, Vec::new());
                    };
                    // Compact the older half of History (light heuristic). At least
                    // one message is always left verbatim so the summary has context.
                    let history_len = e.history.0.len();
                    let upto = (history_len / 2).max(1).min(history_len) as u32;
                    let msgs = e.history.0[..upto as usize].to_vec();
                    let params = e.model.clone().unwrap_or_else(|| world.resources.model.clone());
                    (
                        upto,
                        crate::agent::world::history::History(msgs),
                        params,
                    )
                };
                if let Some(e) = world.entities.get_mut(entity) {
                    e.activity = Activity::Compacting { cmd };
                }
                (
                    world,
                    vec![Command::Compact {
                        cmd,
                        entity: *entity,
                        upto,
                        messages,
                        params,
                        key: CommandKey,
                    }],
                )
            }

            // ----------------------------------------------------------------
            // Compacted: splice the summary over the replaced oldest messages,
            // drain the Inbox, and resume the deferred continuation (→ Thinking
            // + CallModel). The History is now shorter; the next inference sees
            // a summarized past. See docs/agent/world/ecs-runtime.md
            // (CompactionSystem; Compacted splices the summary Msg).
            // ----------------------------------------------------------------
            LogicalInput::Compacted {
                cmd,
                entity,
                summary,
                replaced,
                ..
            } => {
                let compacting_cmd = match world.entities.get(entity) {
                    Some(e) => match e.activity {
                        Activity::Compacting { cmd: c } => c,
                        _ => return (world.clone(), Vec::new()),
                    },
                    None => return (world.clone(), Vec::new()),
                };
                if compacting_cmd != *cmd {
                    return (world.clone(), Vec::new());
                }

                let mut world = world.clone();
                let (cont_cmd, ids) = world.resources.ids.mint_cmd();
                world.resources.ids = ids;
                let default_model = world.resources.model.clone();
                // Re-offer the ROOT (surface-capable) entity its surface tools on the
                // post-compaction continuation so it does not lose `set_value` for the
                // rest of the turn (GAP A). A non-root entity is offered none.
                let surface_tools = if *entity == world.root {
                    world.resources.surface_tools.clone()
                } else {
                    ToolSet::default()
                };

                if let Some(e) = world.entities.get_mut(entity) {
                    // Splice: replace the oldest `replaced` History messages with one
                    // summary Msg. Recent turns are preserved verbatim (Inv 11).
                    let replaced_n = (*replaced as usize).min(e.history.0.len());
                    if replaced_n > 0 && !summary.is_empty() {
                        let tail = e.history.0[replaced_n..].to_vec();
                        e.history.0.clear();
                        e.history.0.push(Msg {
                            role: Role::User,
                            content: summary.clone(),
                        });
                        e.history.0.extend(tail);
                    }
                    // Drain the Inbox into History (FIFO, oldest first — the deferred
                    // continuation messages). The next `CallModel` carries them as the
                    // first new user input after the compacted context.
                    for content in std::mem::take(&mut e.inbox.pending) {
                        e.history.0.push(Msg {
                            role: Role::User,
                            content,
                        });
                    }
                    let params = e.model.clone().unwrap_or(default_model);
                    let messages = e.history.clone();
                    e.activity = Activity::Thinking { cmd: cont_cmd };
                    return (
                        world,
                        vec![Command::CallModel {
                            cmd: cont_cmd,
                            entity: *entity,
                            messages,
                            tools: surface_tools,
                            params,
                            key: CommandKey,
                        }],
                    );
                }
                (world, Vec::new())
            }

            // ----------------------------------------------------------------
            // ModelFailed for a compaction call (entity is Compacting, not
            // Thinking): proceed UN-compacted (best-effort, Inv 10). History is
            // unchanged; drain the Inbox and resume the continuation `CallModel`
            // with the full (un-summarized) History. The context may still be
            // high — that is acceptable; a compaction failure must never stall
            // or deadlock the entity.
            // ----------------------------------------------------------------
            LogicalInput::ModelFailed { cmd, entity, .. } => {
                let compacting_cmd = match world.entities.get(entity) {
                    Some(e) => match e.activity {
                        Activity::Compacting { cmd: c } => c,
                        _ => return (world.clone(), Vec::new()),
                    },
                    None => return (world.clone(), Vec::new()),
                };
                if compacting_cmd != *cmd {
                    return (world.clone(), Vec::new());
                }

                let mut world = world.clone();
                let (cont_cmd, ids) = world.resources.ids.mint_cmd();
                world.resources.ids = ids;
                let default_model = world.resources.model.clone();
                // Re-offer the ROOT (surface-capable) entity its surface tools on the
                // un-compacted continuation (GAP A); a non-root entity is offered none.
                let surface_tools = if *entity == world.root {
                    world.resources.surface_tools.clone()
                } else {
                    ToolSet::default()
                };

                if let Some(e) = world.entities.get_mut(entity) {
                    // Skip summary — proceed with the existing un-compacted History.
                    // Drain the Inbox: same drain as the success path so the same
                    // messages reach the model regardless of compaction outcome.
                    for content in std::mem::take(&mut e.inbox.pending) {
                        e.history.0.push(Msg {
                            role: Role::User,
                            content,
                        });
                    }
                    let params = e.model.clone().unwrap_or(default_model);
                    let messages = e.history.clone();
                    e.activity = Activity::Thinking { cmd: cont_cmd };
                    return (
                        world,
                        vec![Command::CallModel {
                            cmd: cont_cmd,
                            entity: *entity,
                            messages,
                            tools: surface_tools,
                            params,
                            key: CommandKey,
                        }],
                    );
                }
                (world, Vec::new())
            }

            _ => (world.clone(), Vec::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether `entity` is currently `Compacting` on exactly `cmd`. The correlation
/// guard that makes settling idempotent: a stray result for an already-settled
/// cmd is dropped. Used by tests to verify the expected state.
fn _compacting(world: &World, entity: &EntityId, cmd: u32) -> bool {
    world
        .entities
        .get(entity)
        .map(|e| matches!(e.activity, Activity::Compacting { cmd: c } if c == cmd))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::budget::{Budget, Limits};
    use crate::agent::world::effects::{Command, CommandKey};
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, Fingerprint, LogicalInput, ModelError, ModelMeta, Origin,
        ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::systems::tick;
    use crate::agent::world::world::{
        Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources,
        Tick, World,
    };

    fn model() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// Build a World with one primary entity in the given `activity` with the
    /// given `budget` and `inbox`.
    fn world_with(activity: Activity, budget: Budget, inbox: Inbox) -> World {
        let mut world = World::new(0, Resources::new(42, model()));
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History::default(),
                activity,
                gate: EntityGate::default(),
                budget,
                inbox,
                turns: 0,
                model: None,
            },
        );
        world
    }

    /// A `ModelResponded` event for entity 0 with the given `cmd` and `usage`.
    fn model_responded(cmd: CmdId, usage: Usage, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::Text { text: "answer".into() }],
                meta: ModelMeta {
                    usage,
                    model_id: "claude-x".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    /// A `Compacted` event for entity 0 / `cmd` with the given `summary` and
    /// `replaced` count.
    fn compacted(cmd: CmdId, summary: Vec<Block>, replaced: u32, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::Compacted {
                cmd,
                entity: 0,
                fingerprint: Fingerprint(format!("fp-compact-{cmd}")),
                summary,
                replaced,
            },
        }
    }

    /// A `ModelFailed` event for entity 0 / `cmd` (compaction failure path).
    fn model_failed(cmd: CmdId, at: Tick) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: None,
            input: LogicalInput::ModelFailed {
                cmd,
                entity: 0,
                fingerprint: Fingerprint(format!("fp-compact-{cmd}")),
                error: ModelError::Http(500),
            },
        }
    }

    // -----------------------------------------------------------------------
    // VC-P2-compaction.1: context pressure on a pending continuation defers it
    // -----------------------------------------------------------------------

    /// VC-P2-compaction.1: when an entity finishes a turn (`ModelResponded·EndTurn`)
    /// with context over the limit AND the Inbox holds a pending message, the
    /// CompactionSystem defers the continuation: entity → `Compacting` and exactly
    /// one `Compact` Command is emitted. No `CallModel` is emitted (the continuation
    /// is DEFERRED, not started — Inv 10, 11).
    #[test]
    fn context_pressure_on_pending_continuation_defers_to_compacting() {
        // Entity is `Thinking { cmd: 0 }` with a context limit of 50.
        // Inbox has one pending message (a continuation is pending).
        let budget = Budget {
            limits: Limits { spend_limit: 0, context_limit: 50, ..Default::default() },
            ..Budget::default()
        };
        let inbox = Inbox {
            pending: vec![vec![Block::Text { text: "next question".into() }]],
        };
        let world = world_with(Activity::Thinking { cmd: 0 }, budget, inbox);

        // Push entity history so the compaction heuristic has something to compact.
        let world = {
            let mut w = world;
            let e = w.entities.get_mut(&0).unwrap();
            e.history.0.push(Msg {
                role: Role::User,
                content: vec![Block::Text { text: "old user msg".into() }],
            });
            e.history.0.push(Msg {
                role: Role::Assistant,
                content: vec![Block::Text { text: "old assistant reply".into() }],
            });
            w
        };

        // `ModelResponded·EndTurn` with usage that pushes context over the limit.
        // input_tokens=60+output_tokens=0 → context.used=60 > context_limit=50.
        let (world, commands) = tick(
            &world,
            &model_responded(
                0,
                Usage { input_tokens: 60, output_tokens: 0 },
                1,
            ),
        );

        let e = world.entities.get(&0).expect("entity");

        // Entity must be Compacting (continuation deferred, NOT Idle or Thinking).
        let compact_cmd = match e.activity {
            Activity::Compacting { cmd } => cmd,
            ref other => panic!("expected Compacting, got {other:?}"),
        };

        // Exactly one Compact command; NO CallModel (premature continuation absent).
        let compact_cmds: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::Compact { .. }))
            .collect();
        assert_eq!(compact_cmds.len(), 1, "exactly one Compact command");
        assert!(
            !commands.iter().any(|c| matches!(c, Command::CallModel { .. })),
            "no premature CallModel — continuation is deferred (Inv 10)"
        );

        // The Compact command carries the minted cmd, entity, and the history slice.
        match compact_cmds[0] {
            Command::Compact { cmd, entity, upto, .. } => {
                assert_eq!(*cmd, compact_cmd, "Compact carries the compacting cmd");
                assert_eq!(*entity, 0, "Compact targets entity 0");
                assert!(*upto > 0, "at least one message compacted");
            }
            _ => unreachable!(),
        }
    }

    // -----------------------------------------------------------------------
    // VC-P2-compaction.2: Compacted splices the summary and resumes continuation
    // -----------------------------------------------------------------------

    /// VC-P2-compaction.2: on `Compacted { summary, replaced }`, the summary Msg
    /// replaces the oldest `replaced` History messages, the Inbox is drained, the
    /// entity transitions → `Thinking`, and exactly one `CallModel` is emitted
    /// (the deferred continuation, Inv 10, 11).
    #[test]
    fn compacted_splices_summary_and_resumes_continuation() {
        // Entity is Compacting { cmd: 5 }.
        // History has 4 messages; Inbox has one pending message.
        let budget = Budget {
            limits: Limits { spend_limit: 0, context_limit: 50, ..Default::default() },
            ..Budget::default()
        };
        let inbox = Inbox {
            pending: vec![vec![Block::Text { text: "next user msg".into() }]],
        };
        let world = {
            let mut w = world_with(Activity::Compacting { cmd: 5 }, budget, inbox);
            let e = w.entities.get_mut(&0).unwrap();
            for i in 0..4u32 {
                e.history.0.push(Msg {
                    role: if i % 2 == 0 { Role::User } else { Role::Assistant },
                    content: vec![Block::Text { text: format!("msg {i}") }],
                });
            }
            w
        };

        let summary_blocks = vec![Block::Text { text: "summary of old turns".into() }];
        let (world, commands) = tick(
            &world,
            &compacted(5, summary_blocks.clone(), 2, 2),
        );

        let e = world.entities.get(&0).expect("entity");

        // Entity → Thinking (continuation resumed).
        let cont_cmd = match e.activity {
            Activity::Thinking { cmd } => cmd,
            ref other => panic!("expected Thinking, got {other:?}"),
        };

        // Exactly one CallModel (the deferred continuation).
        let call_models: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { .. }))
            .collect();
        assert_eq!(call_models.len(), 1, "exactly one continuation CallModel");
        match call_models[0] {
            Command::CallModel { cmd, entity, messages, .. } => {
                assert_eq!(*cmd, cont_cmd, "CallModel carries the new cmd");
                assert_eq!(*entity, 0);
                // History = [summary_msg, msg2, msg3, inbox_msg].
                // replaced=2 → first 2 messages replaced by one summary Msg.
                assert_eq!(
                    messages.0.len(),
                    4,
                    "history: summary(1) + 2 surviving msgs + 1 inbox msg"
                );
                // First message is the summary.
                assert_eq!(
                    messages.0[0].content,
                    summary_blocks,
                    "first msg is the spliced summary"
                );
                // Last message is the drained inbox message.
                assert_eq!(
                    messages.0[3].content,
                    vec![Block::Text { text: "next user msg".into() }],
                    "last msg is drained from inbox"
                );
            }
            _ => unreachable!(),
        }

        // Inbox is now empty (drained into History).
        assert!(e.inbox.pending.is_empty(), "inbox drained after Compacted");
    }

    // -----------------------------------------------------------------------
    // VC-P2-compaction.3: compaction ModelFailed proceeds un-compacted (best-effort)
    // -----------------------------------------------------------------------

    /// VC-P2-compaction.3: a compaction `ModelFailed` (the `Compact` model call
    /// failed) proceeds UN-compacted: history is unchanged, the Inbox is drained,
    /// the entity transitions → `Thinking`, and exactly one `CallModel` is emitted
    /// (best-effort, Inv 10 — a compaction failure is never a new sink).
    #[test]
    fn compaction_model_failed_proceeds_un_compacted_best_effort() {
        // Entity is Compacting { cmd: 7 }.
        // History has 3 messages; Inbox has one pending message.
        let budget = Budget {
            limits: Limits { spend_limit: 0, context_limit: 50, ..Default::default() },
            ..Budget::default()
        };
        let inbox = Inbox {
            pending: vec![vec![Block::Text { text: "next user msg".into() }]],
        };
        let world = {
            let mut w = world_with(Activity::Compacting { cmd: 7 }, budget, inbox);
            let e = w.entities.get_mut(&0).unwrap();
            for i in 0..3u32 {
                e.history.0.push(Msg {
                    role: if i % 2 == 0 { Role::User } else { Role::Assistant },
                    content: vec![Block::Text { text: format!("msg {i}") }],
                });
            }
            w
        };

        let (world, commands) = tick(&world, &model_failed(7, 2));

        let e = world.entities.get(&0).expect("entity");

        // Entity → Thinking (best-effort continuation reached, never a sink — Inv 10).
        let cont_cmd = match e.activity {
            Activity::Thinking { cmd } => cmd,
            ref other => panic!("expected Thinking, got {other:?}"), // never a sink
        };

        // Exactly one CallModel (un-compacted continuation).
        let call_models: Vec<&Command> = commands
            .iter()
            .filter(|c| matches!(c, Command::CallModel { .. }))
            .collect();
        assert_eq!(call_models.len(), 1, "one CallModel even on compaction failure");
        match call_models[0] {
            Command::CallModel { cmd, entity, messages, .. } => {
                assert_eq!(*cmd, cont_cmd);
                assert_eq!(*entity, 0);
                // History = [msg0, msg1, msg2, inbox_msg] — UNCHANGED (un-compacted).
                assert_eq!(
                    messages.0.len(),
                    4,
                    "history: 3 original msgs + 1 inbox msg (un-compacted)"
                );
                // Last message is the inbox message.
                assert_eq!(
                    messages.0[3].content,
                    vec![Block::Text { text: "next user msg".into() }],
                    "inbox msg drained even on compaction failure"
                );
            }
            _ => unreachable!(),
        }

        // Inbox drained.
        assert!(e.inbox.pending.is_empty(), "inbox drained after ModelFailed");

        // History is un-compacted (3 original messages still there).
        assert_eq!(
            e.history.0.len(),
            4,
            "history has the 3 original msgs + inbox msg (no summary spliced)"
        );
    }

    // -----------------------------------------------------------------------
    // Edge: no compaction when context is not exceeded, even with inbox messages
    // -----------------------------------------------------------------------

    /// No compaction fires when context is under the limit, even if the Inbox
    /// holds pending messages.
    #[test]
    fn no_compaction_when_context_under_limit() {
        let budget = Budget {
            limits: Limits { spend_limit: 0, context_limit: 1_000, ..Default::default() },
            ..Budget::default()
        };
        let inbox = Inbox {
            pending: vec![vec![Block::Text { text: "next".into() }]],
        };
        let world = world_with(Activity::Thinking { cmd: 0 }, budget, inbox);

        // usage = 10 tokens, context_limit = 1000 → far under limit.
        let (world, commands) = tick(
            &world,
            &model_responded(0, Usage { input_tokens: 5, output_tokens: 5 }, 1),
        );

        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Idle),
            "no compaction → entity stays Idle (TurnSystem settled it)"
        );
        assert!(
            !commands.iter().any(|c| matches!(c, Command::Compact { .. })),
            "no Compact when context is under the limit"
        );
    }

    // -----------------------------------------------------------------------
    // Edge: no compaction when inbox is empty (no continuation pending)
    // -----------------------------------------------------------------------

    /// No compaction fires when context is over the limit but the Inbox is empty
    /// (no continuation is pending).
    #[test]
    fn no_compaction_when_inbox_empty() {
        let budget = Budget {
            limits: Limits { spend_limit: 0, context_limit: 50, ..Default::default() },
            ..Budget::default()
        };
        // Inbox is empty — no continuation pending.
        let world = world_with(Activity::Thinking { cmd: 0 }, budget, Inbox::default());

        // usage = 60 tokens → context.used = 60 > context_limit = 50.
        let (world, commands) = tick(
            &world,
            &model_responded(0, Usage { input_tokens: 60, output_tokens: 0 }, 1),
        );

        let e = world.entities.get(&0).expect("entity");
        assert!(
            matches!(e.activity, Activity::Idle),
            "no compaction when inbox is empty → entity stays Idle"
        );
        assert!(
            !commands.iter().any(|c| matches!(c, Command::Compact { .. })),
            "no Compact when inbox is empty (no pending continuation)"
        );
    }
}
