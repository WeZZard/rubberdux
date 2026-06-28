//! driver — the imperative-shell tick-driver loop. See docs/agent/world/ecs-runtime.md
//!
//! [`WorldDriver`] is the IMPERATIVE SHELL that drives the pure World runtime
//! live: it owns the IO capabilities (a [`ModelCaller`], a [`SurfaceDriver`] and
//! an [`EventLog`]) the pure `tick`/Systems are forbidden to touch, and turns a
//! stream of [`LogicalInput`]s into recorded `Event`s + a settled `World`.
//!
//! It is the live counterpart to the offline replay loop in
//! `tests/integration/agent/world_walking_skeleton.rs`, promoted out of the test
//! into a reusable, app-worker-driving module. For each submitted input it:
//!
//!   1. wraps the input in an `Event { origin, edge, at, wall, input }`,
//!   2. APPENDS that Event to the log BEFORE folding it (log-before-apply, Inv 4),
//!   3. folds it through the pure `tick` reducer to get the next `World` + the
//!      tick's emitted `Command`s,
//!   4. hands the Commands to the LIVE driver `drive_live` — which performs the
//!      real effect (model call / surface drive), appends each result `Event`,
//!      and returns it — then folds every result back through `tick`, repeating
//!      until the entity is quiescent (the tool-loop continuation `CallModel`
//!      results re-enter `tick` here), and
//!   5. advances the logical `Tick` monotonically across every appended Event
//!      (deterministic ticks, Inv 8/12).
//!
//! After each input settles it DERIVES `EntryNotification`s from the primary
//! entity's `History` delta (the assistant text the turn produced) so a host/app
//! can still render the agent's reply, exactly as the legacy `AgentLoop`'s
//! `OutputPort` did — without re-introducing the legacy loop.
//!
//! The driver is GENERIC over the three IO traits so it is OFFLINE-TESTABLE: a
//! test injects a `MemoryEventLog`, a stub `ModelCaller`, and a no-op
//! `SurfaceDriver` and drives a full `Idle → Thinking → Idle` turn with no
//! network. The functional core (`tick`, the Systems, `drive_live`) stays pure;
//! this loop is the only place clocks, the network, and the log are touched.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::entry::{Entry, EntryOrigin};
use crate::agent::runtime::port::EntryNotification;
use crate::provider::moonshot::Message;
use crate::error::Error;

use super::effects::{drive_live, ModelCaller, ResultStamp, SurfaceDriver};
use super::event_log::EventLog;
use super::gates::EntityGate;
use super::history::{Block, History, Msg, Role};
use super::inputs::{Event, LogicalInput, Origin};
use super::replay::{outstanding_cmds, restore};
use super::snapshot::{capture, SnapshotStore};
use super::world::{
    Activity, Components, EdgeId, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources, Tick,
    Timestamp, World,
};

/// The primary entity's id. The shell SEEDS this `Idle` entity at startup because
/// no P0 System creates an entity from `SessionStarted` (IntakeSystem only ADMITS
/// a `UserMessage`, and it NO-OPS when its addressed entity is absent or not
/// `Idle`), so genesis is the shell's responsibility. See
/// docs/agent/world/ecs-runtime.md (Genesis; IntakeSystem).
const PRIMARY_ENTITY: EntityId = 0;

/// The single counterpart relationship the P0 driver routes on: the human edge.
/// Host `UserMessage`s and CONVERSATION results (the model call) are stamped onto
/// it deterministically.
const HUMAN_EDGE: EdgeId = 0;

/// The DISTINCT app/surface edge the agent's SURFACE-WRITE results (the `set_value`
/// `ToolReturned`) are stamped onto, kept separate from `HUMAN_EDGE` so the per-edge
/// mode projection (Inv 19) folds the agent's surface drive as `Driven` (agent-only
/// on this edge) while the human conversation edge stays `Assisted` — the design's
/// concurrent per-edge modes. See docs/agent/world/ecs-runtime.md (Mode-as-projection).
const APP_EDGE: EdgeId = 1;

/// The surface-manipulation tools the live whiteboard App offers its surface-capable
/// ROOT entity, so a live model call is told `set_value` EXISTS and can request it
/// (GAP A). The SOLE source of these names: `bootstrap` RECORDS them into the
/// `SessionStarted` header, and the `SessionStarted` fold (SurfaceSystem) seeds them
/// into `Resources.surface_tools`, which the CallModel-emitting Systems read into
/// each turn's `ToolSet`. Recording-then-folding (rather than seeding the live World
/// directly) is what makes a replay reconstruct surface tools identically (Inv 6/7).
/// The milestone's App is the whiteboard, whose sole surface tool is `set_value`
/// (YAGNI — no general tool registry). See docs/agent/world/ecs-runtime.md (Anthropic
/// model-call mapping; SurfaceSystem).
fn surface_tool_names() -> super::effects::ToolSet {
    super::effects::ToolSet(vec!["set_value".to_string()])
}

/// The imperative-shell loop that drives the pure World live.
///
/// Generic over the three IO capabilities so it is offline-testable:
/// - `C: ModelCaller` — the model-call effect (`MessagesClient` in production),
/// - `S: SurfaceDriver` — the surface-drive sink (the app-worker forwarder), and
/// - `L: EventLog` — the append-only log (`FilesystemEventLog` in production).
pub struct WorldDriver<C, S, L> {
    /// The current settled World, rebuilt by folding every appended Event.
    world: World,
    /// The model-call capability the LIVE driver dispatches `CallModel` through.
    client: C,
    /// The surface-drive sink the LIVE driver forwards `set_value` `RunTool`s to.
    surface_driver: S,
    /// The append-only event log; the sole source of truth (Inv 9).
    log: L,
    /// The monotonic logical tick the NEXT appended Event is stamped at. Advanced
    /// once per appended Event so ticks never depend on scheduling (Inv 8/12).
    next_at: Tick,
    /// How many of the primary entity's `History` messages have already been
    /// emitted as `EntryNotification`s; the watermark for the history-delta diff.
    history_emitted: usize,
    /// The snapshot store the periodic writer captures into, rooted at the
    /// session dir's `snapshots/`. `None` for a `bootstrap`ed session (no
    /// periodic snapshots); `Some` once `open` attaches the session's store so
    /// a resumed long session stays bounded on disk (Inv 11). See
    /// docs/agent/world/ecs-runtime.md — Restore snapshots.
    store: Option<SnapshotStore>,
    /// The logical tick of the last periodic snapshot (or the resume baseline) —
    /// the low watermark each tick advance is checked against to decide whether
    /// it crossed a `Resources.caps.snapshot_interval` boundary. See
    /// docs/agent/world/ecs-runtime.md — Restore snapshots (periodic snapshot).
    last_snapshot_at: Tick,
}

impl<C, S, L> WorldDriver<C, S, L>
where
    C: ModelCaller,
    S: SurfaceDriver,
    L: EventLog,
{
    /// Bootstrap a fresh-session driver: build the genesis `World` (a single
    /// primary `Idle` entity seeded from `seed`, with `model` as the world-default
    /// config), record the `SessionStarted` header at tick 0 — carrying BOTH the
    /// RNG `seed` AND the App's `surface_tools`, the two hidden inputs that must
    /// cross a recorded boundary (Inv 9) — and fold it. The fold (SurfaceSystem)
    /// is what seeds `Resources.surface_tools`; bootstrap RECORDS the tools but no
    /// longer seeds the World directly, keeping ONE source of truth so the live
    /// World and a replay reconstruct surface tools identically. The driver is then
    /// ready to accept inputs starting at tick 1.
    pub fn bootstrap(
        seed: u64,
        model: ModelConfig,
        client: C,
        surface_driver: S,
        mut log: L,
    ) -> Result<Self, Error> {
        let world = genesis_world(seed, model);

        // The session header carries the hidden inputs that must cross a recorded
        // boundary: the RNG `seed` and the App's `surface_tools` (the whiteboard's
        // `set_value`). Recording the tools here — and letting the fold seed
        // `Resources.surface_tools` — is the SINGLE source of truth (GAP A). Record
        // at tick 0 BEFORE folding (Inv 4/9). Offline genesis (the replay tests)
        // records empty tools, so their requests stay byte-identical to a pre-tools call.
        let session = Event {
            origin: Origin::System,
            edge: HUMAN_EDGE,
            at: 0,
            wall: now_wall(),
            input: LogicalInput::SessionStarted {
                seed,
                surface_tools: surface_tool_names().0,
            },
        };
        log.append(&session)?;
        // Folding SessionStarted seeds `Resources.surface_tools` from the recorded
        // names (SurfaceSystem) and advances the logical clock.
        let (world, _no_commands) = super::systems::tick(&world, &session);

        Ok(Self {
            world,
            client,
            surface_driver,
            log,
            next_at: 1,
            history_emitted: 0,
            // A fresh `bootstrap` has no session store yet; the periodic writer
            // is inert until `open` (or a future store-bearing worker) attaches
            // one. Genesis itself (tick 0) is trivially regenerable and never
            // periodically snapshotted.
            store: None,
            last_snapshot_at: 0,
        })
    }

    /// Open an EXISTING session: reconstruct the World from its recorded log and
    /// continue driving it live, instead of seeding a fresh genesis. This is the
    /// RESUME counterpart to [`bootstrap`](Self::bootstrap) — the runtime can now
    /// reopen a restarted session rather than only ever starting over (US-7).
    ///
    /// An EMPTY log is a fresh session, so it delegates to `bootstrap` (genesis +
    /// recorded `SessionStarted`) and merely attaches `store`.
    ///
    /// A NON-EMPTY log RESUMES via [`restore`](super::replay::restore) — the
    /// nearest snapshot ≤ the last recorded tick, plus a tail fold, plus a
    /// bounded resume over the reconstructed World's outstanding effects (Inv
    /// 5/9/17) — and then:
    ///
    ///  - folds any `Settled` crash-tail reconciliation `restore` appended to the
    ///    log and drives the retry Commands it re-emits (e.g. a fresh, accounted
    ///    model call after a `Thinking` crash settles `→ Idle`) to live
    ///    quiescence, so a resumed session does not merely reconstruct but
    ///    CONTINUES. `Settled` covers the `CallModel`/`RunTool` crash tails. The
    ///    remaining `Redispatch`-class reconciliations re-issue under the same
    ///    idempotency key, and COMPACTION IS one of them that this driver DOES
    ///    dispatch live (`CompactionSystem` is registered and `drive_live` issues
    ///    `Command::Compact`), so a `Compact` `Redispatch` IS reachable on a crash
    ///    tail. `effects::resume` does not LOG a `Redispatch`, so
    ///    `resume_reconciliations` (which folds only newly-logged events) cannot
    ///    settle it, leaving the entity reconstructed in `Compacting`. This P0
    ///    driver cannot yet reconstruct the `Compact` request payload to re-issue
    ///    it, so `open` FAILS LOUDLY on such a tail rather than opening with a
    ///    silently-stranded entity — a documented P0 limitation; full
    ///    compaction-resume re-dispatch is deferred. peer / timer / human-action
    ///    are never dispatched by this driver, so those `Redispatch` kinds do not
    ///    arise on this path,
    ///  - sets `next_at` to the reconstructed `World.clock + 1` so ticks continue
    ///    monotonically past everything already recorded (Inv 8/12), and
    ///  - sets `history_emitted` to the reconstructed primary entity's `History`
    ///    length so the first post-resume turn does NOT re-emit assistant text
    ///    the pre-restart run already delivered.
    ///
    /// The reconstruction reuses the SAME genesis (seed from the recorded
    /// `SessionStarted` header, `model` as the world default) the live run used,
    /// so it is byte-identical to a full replay (Inv 6). `store` is rooted at the
    /// session dir's `snapshots/`; from here the periodic writer captures the
    /// World there each time a tick advance crosses `Resources.caps.snapshot_interval`.
    ///
    /// See docs/agent/world/ecs-runtime.md — Restore snapshots (RESTORE).
    //
    // No production caller until CR-worker-resume wires `run_app_worker` to
    // resume the latest session via `open` (today the worker only `bootstrap`s),
    // so the bin target sees `open` and its resume-only helpers as unused until
    // then — the same dead-code allowance the sibling replay/snapshot spine
    // carries while its callers are still landing.
    #[allow(dead_code)]
    pub async fn open(
        seed: u64,
        model: ModelConfig,
        client: C,
        surface_driver: S,
        log: L,
        store: SnapshotStore,
    ) -> Result<Self, Error> {
        let events = log.load()?;

        // An empty log is a fresh session: seed genesis exactly as `bootstrap`
        // does, then arm the periodic writer with the session's store.
        if events.is_empty() {
            let mut driver = Self::bootstrap(seed, model, client, surface_driver, log)?;
            driver.store = Some(store);
            return Ok(driver);
        }

        // A non-empty log RESUMES: reconstruct from the nearest snapshot + tail +
        // bounded resume, continuing at the recorded frontier (never re-genesis).
        let lifecycle = log.load_lifecycle()?;
        let target = events.iter().map(|e| e.at).max().ok_or_else(|| {
            Error::World("a non-empty session log carries no recorded tick".into())
        })?;

        let mut log = log;
        let restored = restore(
            &store,
            &events,
            &lifecycle,
            |seed| genesis_world(seed, model.clone()),
            target,
            &mut log,
        )?;

        let mut driver = Self {
            world: restored,
            client,
            surface_driver,
            log,
            // Set after the resume drive below settles the World.
            next_at: 0,
            history_emitted: 0,
            store: Some(store),
            last_snapshot_at: 0,
        };

        // Fold any `Settled` crash-tail reconciliation `restore` appended beyond
        // the loaded tail and drive any retry Commands to live quiescence.
        driver.resume_reconciliations(events.len()).await?;

        // Crash-mid-compaction guard (P0 limitation). `restore`'s bounded resume
        // reconciles each outstanding effect per its `EffectKind`: a `CallModel` /
        // `RunTool` tail settles to a LOGGED stratum-1 result that
        // `resume_reconciliations` just folded in, but a `Compact` tail reconciles
        // to a `Redispatch` (re-dispatch under the same key) that `effects::resume`
        // does NOT log — so nothing was folded for it and the entity is still
        // `Compacting`. This driver cannot yet reconstruct the `Compact` request
        // payload (the dispatch's `upto` / `messages` / `params` are not carried in
        // `Activity::Compacting { cmd }`), so rather than open a driver with an
        // entity silently stranded `Compacting` forever, fail LOUDLY. Any cmd still
        // outstanding after resume is, by construction, such an un-re-issued
        // `Redispatch` — the only effect kind this P0 driver dispatches that resume
        // cannot settle (peer / timer / human-action are never dispatched). Full
        // compaction-resume re-dispatch is deferred. See
        // docs/agent/world/ecs-runtime.md — Reconciliation (Inv 5/17).
        let unsettled = outstanding_cmds(&driver.world);
        if let Some(&cmd) = unsettled.iter().next() {
            let entity = driver
                .world
                .entities
                .iter()
                .find(|(_, components)| {
                    matches!(components.activity, Activity::Compacting { cmd: held } if held == cmd)
                })
                .map(|(id, _)| *id);
            return Err(Error::World(format!(
                "crash-mid-compaction resume is not yet supported: entity {entity:?} is \
                 reconstructed in Compacting with an outstanding Compact cmd {cmd}, which the \
                 bounded resume reconciled to a Redispatch this P0 driver does not re-issue; \
                 failing loudly rather than opening with a silently-stranded entity (full \
                 compaction-resume re-dispatch is deferred)"
            )));
        }

        // Continue past everything recorded, and suppress re-emitting old text.
        driver.next_at = driver.world.clock + 1;
        driver.history_emitted = driver.primary_history_len();
        Ok(driver)
    }

    /// Submit one logical input, drive the World to quiescence, and return the
    /// `EntryNotification`s the turn produced.
    ///
    /// Wraps the input in its `Event` envelope, APPENDS it (log-before-apply,
    /// Inv 4), folds it through `tick`, then drains the emitted Commands through
    /// the LIVE driver — re-folding each result `Event` until no Commands remain
    /// (the tool-loop continuation `CallModel` results re-enter `tick` here).
    /// Finally it derives the new assistant `History` as `EntryNotification`s.
    pub async fn submit(&mut self, input: LogicalInput) -> Result<Vec<EntryNotification>, Error> {
        let event = Event {
            origin: origin_for(&input),
            edge: HUMAN_EDGE,
            at: self.next_at,
            wall: now_wall(),
            input,
        };
        self.next_at += 1;

        // Log-before-apply (Inv 4): durable BEFORE the reducer folds it.
        self.log.append(&event)?;
        let (next, commands) = super::systems::tick(&self.world, &event);
        self.world = next;
        self.maybe_snapshot();

        self.drive_to_quiescence(commands).await?;
        Ok(self.derive_entry_notifications())
    }

    /// Drive a tick's emitted Commands to quiescence: dispatch each batch through
    /// `drive_live` (real effect + log-before-apply), fold every result back
    /// through `tick`, and repeat while the fold keeps emitting Commands. Each
    /// `drive_live` call drives ONE tick's worth of effects and stamps its results
    /// at a fresh monotonic tick.
    async fn drive_to_quiescence(&mut self, mut commands: Vec<crate::agent::world::effects::Command>) -> Result<(), Error> {
        while !commands.is_empty() {
            let stamp = ResultStamp {
                edge: HUMAN_EDGE,
                // Agent surface-write results route to the DISTINCT app edge so
                // `mode(APP_EDGE)` folds Driven, independent of the human edge (GAP B).
                app_edge: APP_EDGE,
                at: self.next_at,
                wall: now_wall(),
            };
            self.next_at += 1;

            // The LIVE driver performs the real effect and appends each result
            // Event to the log BEFORE returning it (log-before-apply, Inv 4). The
            // surfaces it fingerprints over are the World's perceived view, so a
            // re-emitted UI request diverges exactly when that surface changed
            // (Inv 18 / Theme 2b).
            let results = drive_live(
                &commands,
                stamp,
                &self.world.resources.surfaces,
                &self.client,
                &self.surface_driver,
                &mut self.log,
            )
            .await?;

            commands = Vec::new();
            for result in &results {
                let (next, mut cmds) = super::systems::tick(&self.world, result);
                self.world = next;
                self.maybe_snapshot();
                commands.append(&mut cmds);
            }
        }
        Ok(())
    }

    /// Fold the `Settled` reconciliations [`restore`](super::replay::restore)
    /// appended beyond the loaded tail (indices `original_len..` of the now
    /// re-loaded log) into the reconstructed World, then drive any Commands they
    /// re-emit — e.g. an Autonomy/budget retry after a crash-cancelled turn
    /// settles `Thinking → Idle` — to live quiescence. A clean resume appended
    /// nothing, so this is a no-op. The reconstructed-plus-reconciled frontier
    /// becomes both the next-tick origin and the snapshot baseline, so the
    /// periodic writer does not re-capture the just-restored World. See
    /// docs/agent/world/ecs-runtime.md — Reconciliation (Inv 5/17).
    // Reachable only through `open`, whose production caller lands in CR-worker-resume.
    #[allow(dead_code)]
    async fn resume_reconciliations(&mut self, original_len: usize) -> Result<(), Error> {
        let reloaded = self.log.load()?;
        let appended: Vec<Event> = reloaded
            .get(original_len..)
            .map(|tail| tail.to_vec())
            .unwrap_or_default();

        let mut commands = Vec::new();
        for ev in &appended {
            let (next, mut cmds) = super::systems::tick(&self.world, ev);
            self.world = next;
            commands.append(&mut cmds);
        }

        self.next_at = self.world.clock + 1;
        self.last_snapshot_at = self.world.clock;
        self.drive_to_quiescence(commands).await
    }

    /// Capture and persist a World snapshot when the latest tick advance crossed
    /// a `Resources.caps.snapshot_interval` boundary. A snapshot is a DERIVED
    /// CACHE — the log is the single source of truth (Inv 9) — so a write failure
    /// is logged and the turn continues rather than aborting; the skipped bucket
    /// is regenerable from the log (restore falls back to an earlier snapshot or
    /// genesis). A driver without a store (a `bootstrap`ed session) or a zero
    /// interval (unbounded) never snapshots. See docs/agent/world/ecs-runtime.md
    /// — Restore snapshots (periodic snapshot).
    fn maybe_snapshot(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let interval = self.world.resources.caps.snapshot_interval;
        let clock = self.world.clock;
        if !crosses_snapshot_boundary(self.last_snapshot_at, clock, interval) {
            return;
        }
        if let Err(e) = store.write(&capture(self.world.clone())) {
            log::warn!(
                "periodic snapshot at tick {clock} failed (derived cache; the log \
                 remains the source of truth): {e}"
            );
        }
        // Advance the baseline whether or not the write succeeded: a skipped
        // bucket is regenerable, and not advancing would retry every tick.
        self.last_snapshot_at = clock;
    }

    /// The reconstructed primary entity's `History` length — the watermark
    /// `history_emitted` resumes from so already-delivered assistant text is not
    /// re-notified after a restart.
    // Reachable only through `open`, whose production caller lands in CR-worker-resume.
    #[allow(dead_code)]
    fn primary_history_len(&self) -> usize {
        self.world
            .entities
            .get(&PRIMARY_ENTITY)
            .map_or(0, |entity| entity.history.0.len())
    }

    /// Derive `EntryNotification`s from the primary entity's `History` delta since
    /// the last call: each NEW assistant message carrying text becomes one
    /// `EntryNotification` so a host/app renders the agent's reply, mirroring the
    /// legacy `AgentLoop`'s `OutputPort`. The entry `id` is the message's stable
    /// position in `History`; `parent_id` is its predecessor. Messages without
    /// assistant text (pure tool_use turns, user turns the app already shows) are
    /// not emitted. The watermark advances past every inspected message so each is
    /// emitted at most once.
    fn derive_entry_notifications(&mut self) -> Vec<EntryNotification> {
        let mut out = Vec::new();
        let Some(entity) = self.world.entities.get(&PRIMARY_ENTITY) else {
            return out;
        };
        let messages = &entity.history.0;
        for index in self.history_emitted..messages.len() {
            let message = &messages[index];
            if !matches!(message.role, Role::Assistant) {
                continue;
            }
            let Some(text) = assistant_text(message) else {
                continue;
            };
            out.push(EntryNotification {
                entry: Entry {
                    id: index,
                    parent_id: index.checked_sub(1),
                    message: Message::Assistant {
                        content: Some(text),
                        reasoning_content: None,
                        tool_calls: None,
                        partial: None,
                    },
                    origin: EntryOrigin::Assistant,
                    channel_metadata: None,
                },
                // The turn has settled (quiescent) by the time this runs, so the
                // derived assistant text is final.
                is_final: true,
            });
        }
        self.history_emitted = messages.len();
        out
    }
}

/// The genesis `World` a session starts from: tick 0, a single primary `Idle`
/// entity seeded from `seed` with `model` as the world default — BEFORE the
/// `SessionStarted` header is folded. Shared by `bootstrap` (which then records
/// and folds the header) and `open`'s restore (whose `build_genesis` reseeds the
/// world from the recorded header's seed, Inv 8), so a live run and a
/// reconstruction start from byte-identical genesis (Inv 6). See
/// docs/agent/world/ecs-runtime.md (Genesis).
fn genesis_world(seed: u64, model: ModelConfig) -> World {
    let mut world = World::new(PRIMARY_ENTITY, Resources::new(seed, model));
    world
        .entities
        .insert(PRIMARY_ENTITY, primary_idle_components());
    world
}

/// Whether advancing the logical clock from `last` to `current` crossed an
/// `interval`-tick snapshot boundary. `interval == 0` disables periodic
/// snapshots (an unbounded cap), and a non-advancing clock never crosses. See
/// docs/agent/world/ecs-runtime.md §"Boundedness"/Caps.
fn crosses_snapshot_boundary(last: Tick, current: Tick, interval: Tick) -> bool {
    interval != 0 && current > last && current / interval > last / interval
}

/// The genesis primary entity: `Identity::Primary`, root lineage, empty History,
/// `Idle`. Mirrors the `idle_world()` fixtures so the live World starts exactly
/// where the Systems' unit tests assume. See docs/agent/world/ecs-runtime.md.
fn primary_idle_components() -> Components {
    Components {
        identity: Identity::Primary,
        lineage: Lineage {
            parent: None,
            depth: 0,
        },
        history: History::default(),
        activity: Activity::Idle,
        gate: EntityGate::default(),
        budget: crate::agent::world::budget::Budget::default(),
        inbox: Inbox::default(),
        turns: 0,
        model: None,
    }
}

/// The `Event.origin` a submitted input is stamped with. A host `UserMessage` and
/// a human surface mutation are `Human`-origin free variables (Inv 18); a surface
/// observation is a `System` perception. Everything else is `System`-neutral.
fn origin_for(input: &LogicalInput) -> Origin {
    match input {
        LogicalInput::UserMessage { .. } | LogicalInput::SurfaceMutated { .. } => Origin::Human,
        _ => Origin::System,
    }
}

/// Concatenate an assistant message's `Text` blocks (newline-joined), returning
/// `None` when the message carries no text (e.g. a pure tool_use turn).
fn assistant_text(message: &Msg) -> Option<String> {
    let mut text = String::new();
    for block in &message.content {
        if let Block::Text { text: chunk } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(chunk);
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The wall-time the shell observes when it appends an Event — the recorded
/// hidden-input boundary the agent reads as data (Inv 2). The PURE core never
/// calls a clock; the shell records it here so replay reproduces it from the log.
/// A clock before the epoch (unreachable in practice) records `None`.
fn now_wall() -> Option<Timestamp> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as Timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::effects::Command;
    use crate::agent::world::event_log::MemoryEventLog;
    use crate::agent::world::inputs::{
        Capabilities, ModelMeta, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::world::Effort;
    use serde_json::Value as Json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `ModelCaller` that returns a fixed `EndTurn` assistant text and counts its
    /// invocations, so a test proves EXACTLY ONE model call happened (one
    /// `Idle → Thinking → Idle` turn) without a network.
    struct StubModelCaller {
        text: String,
        calls: AtomicUsize,
    }

    impl ModelCaller for StubModelCaller {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok((
                vec![Block::Text {
                    text: self.text.clone(),
                }],
                ModelMeta {
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    },
                    model_id: "claude-stub".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            ))
        }
    }

    /// A `SurfaceDriver` that fails loudly if reached: a model-only turn never
    /// drives a surface, so a `CallModel`-only path proves the sink is untouched.
    struct NoSurfaceDrive;

    impl SurfaceDriver for NoSurfaceDrive {
        async fn drive(&self, _command: &Command) -> Result<(), Error> {
            panic!("a CallModel-only turn must not reach the surface driver");
        }
    }

    fn stub_model() -> ModelConfig {
        ModelConfig {
            model: "claude-stub".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    /// The acceptance self-check: a `UserMessage` drives the primary entity
    /// `Idle → Thinking → Idle`, the log records `SessionStarted` + `UserMessage` +
    /// `ModelResponded` in order, and an `EntryNotification` carrying the assistant
    /// text is emitted. Offline: `MemoryEventLog` + stub `ModelCaller` + no-op
    /// `SurfaceDriver`, no network.
    #[tokio::test]
    async fn user_message_drives_idle_to_thinking_to_idle_logs_and_emits_entry() {
        let assistant_text = "pong";
        let client = StubModelCaller {
            text: assistant_text.into(),
            calls: AtomicUsize::new(0),
        };

        let mut driver =
            WorldDriver::bootstrap(7, stub_model(), client, NoSurfaceDrive, MemoryEventLog::new())
                .expect("bootstrap the driver");

        // Pre-condition: the seeded primary entity starts Idle.
        assert!(
            matches!(
                driver.world.entities.get(&PRIMARY_ENTITY).unwrap().activity,
                Activity::Idle
            ),
            "the seeded primary entity starts Idle"
        );

        // bootstrap RECORDS the App's surface tools into SessionStarted and the
        // fold (SurfaceSystem) seeds `Resources.surface_tools` — bootstrap never
        // seeds the World directly, yet the live World still carries `set_value`.
        assert_eq!(
            driver.world.resources.surface_tools,
            crate::agent::world::effects::ToolSet(vec!["set_value".into()]),
            "the SessionStarted fold reconstructs Resources.surface_tools (single source of truth)"
        );
        assert!(
            matches!(
                driver.log.load().expect("load log").first().map(|e| &e.input),
                Some(LogicalInput::SessionStarted { surface_tools, .. })
                    if surface_tools == &vec!["set_value".to_string()]
            ),
            "the recorded SessionStarted header carries the App's surface tools"
        );

        let notifications = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "ping".into(),
            })
            .await
            .expect("the turn drives to quiescence");

        // The turn settled back to Idle: Idle → Thinking (Intake's CallModel) →
        // Idle (EndTurn). Exactly ONE model call proves the single Thinking turn.
        let entity = driver.world.entities.get(&PRIMARY_ENTITY).unwrap();
        assert!(
            matches!(entity.activity, Activity::Idle),
            "the turn returns Thinking -> Idle"
        );
        assert_eq!(
            driver.client.calls.load(Ordering::SeqCst),
            1,
            "exactly one model call (one Idle->Thinking->Idle turn)"
        );

        // History holds the user turn then the assistant reply.
        assert_eq!(entity.history.0.len(), 2, "History holds user + assistant");
        assert!(matches!(
            &entity.history.0[1],
            Msg { role: Role::Assistant, content }
                if matches!(content.as_slice(), [Block::Text { text }] if text == assistant_text)
        ));

        // The log records SessionStarted, then UserMessage, then ModelResponded.
        let events = driver.log.load().expect("load the recorded log");
        assert_eq!(events.len(), 3, "SessionStarted + UserMessage + ModelResponded");
        assert!(matches!(events[0].input, LogicalInput::SessionStarted { .. }));
        assert!(matches!(events[1].input, LogicalInput::UserMessage { .. }));
        assert!(matches!(events[2].input, LogicalInput::ModelResponded { .. }));

        // Ticks advance monotonically across every appended Event (Inv 8/12).
        assert_eq!(events[0].at, 0);
        assert_eq!(events[1].at, 1);
        assert_eq!(events[2].at, 2);

        // An EntryNotification carrying the assistant text was derived from the
        // History delta so the app can render the agent's reply.
        assert_eq!(notifications.len(), 1, "one assistant EntryNotification");
        let note = &notifications[0];
        assert!(note.is_final, "a settled turn's assistant text is final");
        assert_eq!(note.entry.origin, EntryOrigin::Assistant);
        assert_eq!(
            note.entry.message.content_text(),
            assistant_text,
            "the notification carries the assistant text"
        );
    }

    /// A second submitted input only emits the NEW assistant text — the
    /// history-delta watermark suppresses re-emitting earlier turns.
    #[tokio::test]
    async fn second_turn_emits_only_the_new_assistant_text() {
        let client = StubModelCaller {
            text: "first".into(),
            calls: AtomicUsize::new(0),
        };
        let mut driver =
            WorldDriver::bootstrap(7, stub_model(), client, NoSurfaceDrive, MemoryEventLog::new())
                .expect("bootstrap");

        let first = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "one".into(),
            })
            .await
            .expect("first turn");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].entry.message.content_text(), "first");

        let second = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "two".into(),
            })
            .await
            .expect("second turn");
        // Only the second turn's assistant text — never a re-emit of the first.
        assert_eq!(second.len(), 1, "only the new assistant message is emitted");
        assert_eq!(second[0].entry.message.content_text(), "first");
        assert_eq!(
            second[0].entry.id, 3,
            "the entry id is the new message's History position (user0, asst1, user2, asst3)"
        );
    }

    /// A `ModelCaller` that drives a tool turn then ends: it returns a `set_value`
    /// tool_use on its FIRST call (so the agent performs a surface write) and an
    /// `EndTurn` text reply on its second (so the turn settles). Counts its calls so
    /// the test proves exactly two model calls bracket the one surface write.
    struct ToolThenEndCaller {
        calls: AtomicUsize,
    }

    impl ModelCaller for ToolThenEndCaller {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let (blocks, stop_reason) = if n == 0 {
                (
                    vec![Block::ToolUse {
                        id: "tu_set_value".into(),
                        name: "set_value".into(),
                        input: serde_json::json!({
                            "surface_ops": [
                                { "op": "set_value", "surface": 9, "element": 2, "value": "agent typed", "base_version": null }
                            ]
                        }),
                    }],
                    StopReason::ToolUse,
                )
            } else {
                (vec![Block::Text { text: "done".into() }], StopReason::EndTurn)
            };
            Ok((
                blocks,
                ModelMeta {
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    },
                    model_id: "claude-stub".into(),
                    stop_reason,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            ))
        }
    }

    /// A `SurfaceDriver` that ACCEPTS every drive (the macOS-app stand-in succeeds),
    /// so the live `set_value` executor records its `ToolReturned` without a real app.
    struct AcceptingSurfaceDriver;

    impl SurfaceDriver for AcceptingSurfaceDriver {
        async fn drive(&self, _command: &Command) -> Result<(), Error> {
            Ok(())
        }
    }

    /// (B) GAP B: when the live driver executes an agent `set_value` surface write, it
    /// stamps that `ToolReturned` on the DISTINCT app edge — so `mode(APP_EDGE)` folds
    /// `Driven` (agent-only on that edge) while the human conversation edge stays
    /// `Assisted` (the Human `UserMessage` + the Agent model replies), realizing the
    /// design's concurrent per-edge modes (Inv 19). Offline: stub model + accepting
    /// surface driver + `MemoryEventLog`, no network and no macOS app.
    #[tokio::test]
    async fn agent_surface_write_folds_driven_on_the_app_edge_human_edge_unaffected() {
        use crate::agent::world::mode::{mode, Mode};

        let mut driver = WorldDriver::bootstrap(
            7,
            stub_model(),
            ToolThenEndCaller {
                calls: AtomicUsize::new(0),
            },
            AcceptingSurfaceDriver,
            MemoryEventLog::new(),
        )
        .expect("bootstrap the driver");

        driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "type into the field".into(),
            })
            .await
            .expect("the surface-driving turn drives to quiescence");

        // Exactly two model calls bracket the one surface write (ToolUse then EndTurn).
        assert_eq!(
            driver.client.calls.load(Ordering::SeqCst),
            2,
            "two model calls (ToolUse → set_value → EndTurn) bracket the surface write"
        );

        let events = driver.log.load().expect("load the recorded log");

        // The agent's surface write is recorded on the APP edge — exactly one
        // agent-origin `ToolReturned` there (the agent UI write's sole fact, Inv 18).
        let app_writes: Vec<&Event> = events
            .iter()
            .filter(|e| e.edge == APP_EDGE && matches!(e.input, LogicalInput::ToolReturned { .. }))
            .collect();
        assert_eq!(app_writes.len(), 1, "the set_value write lands on the app edge");
        assert_eq!(app_writes[0].origin, Origin::Agent, "the write is agent-origin");

        // No surface write contaminates the human conversation edge.
        assert!(
            !events.iter().any(
                |e| e.edge == HUMAN_EDGE && matches!(e.input, LogicalInput::ToolReturned { .. })
            ),
            "no surface write is stamped on the human conversation edge (GAP B)"
        );

        // mode(APP_EDGE) folds Driven (agent-only); the human edge is NOT Driven —
        // the Human `UserMessage` keeps it Assisted (concurrent per-edge modes).
        let window = 16;
        assert_eq!(
            mode(&events, APP_EDGE, window),
            Mode::Driven,
            "the agent-only app edge folds Driven (GAP B / Inv 19)"
        );
        assert_eq!(
            mode(&events, HUMAN_EDGE, window),
            Mode::Assisted,
            "the human conversation edge stays Assisted (Human + Agent), never Driven"
        );
    }

    // --- CR-driver-open: resume reconstruction + periodic snapshot ------------

    /// A `ModelCaller` that PANICS if invoked: a CLEAN resume reconstructs the
    /// World from the recorded log alone (zero model calls), so on `open` this
    /// client is structurally unreachable — proving `open` never re-runs the
    /// recorded turn.
    struct ExplodingModelCaller;

    impl ModelCaller for ExplodingModelCaller {
        async fn call(&self, _request_body: Json) -> Result<(Vec<Block>, ModelMeta), Error> {
            panic!("a clean resume reconstructs from the log alone; it must never call the model");
        }
    }

    /// Record a one-turn session via `bootstrap` + `submit`, then re-materialize
    /// its recorded log into a FRESH in-memory log. Returns the recorded driver's
    /// settled World plus the `next_at` / `History`-length watermarks `open` must
    /// reproduce, alongside the replayable log to reopen from.
    async fn record_one_turn(assistant_text: &str) -> (World, Tick, usize, MemoryEventLog) {
        let client = StubModelCaller {
            text: assistant_text.into(),
            calls: AtomicUsize::new(0),
        };
        let mut driver =
            WorldDriver::bootstrap(7, stub_model(), client, NoSurfaceDrive, MemoryEventLog::new())
                .expect("bootstrap the recording driver");
        driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "ping".into(),
            })
            .await
            .expect("record one turn");

        let world = driver.world.clone();
        let next_at = driver.next_at;
        let history_len = driver
            .world
            .entities
            .get(&PRIMARY_ENTITY)
            .expect("primary entity")
            .history
            .0
            .len();

        // Re-materialize BOTH strata so the reopened driver loads an identical log.
        let mut replay_log = MemoryEventLog::new();
        for event in &driver.log.load().expect("load recorded events") {
            replay_log.append(event).expect("re-append event");
        }
        for record in &driver.log.load_lifecycle().expect("load recorded lifecycle") {
            replay_log
                .append_lifecycle(record)
                .expect("re-append lifecycle");
        }
        (world, next_at, history_len, replay_log)
    }

    /// VC-4.1 (lib-scope): `open` on a NON-EMPTY recorded log reconstructs the
    /// prior World BYTE-IDENTICALLY (never re-genesis), continues at the
    /// reconstructed `clock + 1`, and sets the history watermark so old assistant
    /// text is not re-emitted — all with ZERO model calls (the
    /// `ExplodingModelCaller` is structurally unreachable).
    #[tokio::test]
    async fn open_reconstructs_prior_world_and_continues_at_the_right_tick() {
        let (recorded_world, recorded_next_at, recorded_history_len, replay_log) =
            record_one_turn("pong").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let driver = WorldDriver::open(
            7,
            stub_model(),
            ExplodingModelCaller,
            NoSurfaceDrive,
            replay_log,
            store,
        )
        .await
        .expect("open resumes the recorded session");

        assert_eq!(
            serde_json::to_vec(&driver.world).expect("serialize reopened World"),
            serde_json::to_vec(&recorded_world).expect("serialize recorded World"),
            "open reconstructs the prior World byte-identically (no re-genesis)"
        );
        assert_eq!(
            driver.next_at, recorded_next_at,
            "open continues at the reconstructed clock + 1"
        );
        assert_eq!(
            driver.history_emitted, recorded_history_len,
            "the history watermark suppresses re-emitting already-delivered text"
        );

        // No snapshot is written during a short resume (clock never crosses the
        // default interval of 64) — restore reconstructs from the log directly.
        assert!(
            driver
                .store
                .as_ref()
                .expect("the resumed driver owns a store")
                .list_ticks()
                .expect("list snapshot ticks")
                .is_empty(),
            "a short resume writes no periodic snapshot"
        );
    }

    /// A resumed driver continues the tick sequence monotonically past the
    /// recorded frontier and emits ONLY the new turn's assistant text — the
    /// reconstructed history watermark suppresses re-notifying the pre-restart
    /// reply.
    #[tokio::test]
    async fn resumed_driver_emits_only_new_text_and_continues_ticks() {
        let (_recorded_world, recorded_next_at, _history_len, replay_log) =
            record_one_turn("first").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let mut driver = WorldDriver::open(
            7,
            stub_model(),
            StubModelCaller {
                text: "second".into(),
                calls: AtomicUsize::new(0),
            },
            NoSurfaceDrive,
            replay_log,
            store,
        )
        .await
        .expect("open resumes the recorded session");

        let notifications = driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "again".into(),
            })
            .await
            .expect("the post-resume turn drives to quiescence");

        assert_eq!(
            notifications.len(),
            1,
            "only the new turn's assistant text is emitted, never a re-emit of the recorded reply"
        );
        assert_eq!(
            notifications[0].entry.message.content_text(),
            "second",
            "the notification carries the NEW assistant text"
        );

        let events = driver.log.load().expect("load the resumed log");
        // Recorded: SessionStarted@0, UserMessage@1, ModelResponded@2 → resume @3.
        assert_eq!(
            events.len(),
            5,
            "the resumed turn appends UserMessage + ModelResponded after the recorded three"
        );
        assert_eq!(
            events[3].at, recorded_next_at,
            "the resumed UserMessage continues at the reconstructed clock + 1"
        );
        assert!(
            events[3].at > events[2].at,
            "resumed ticks never collide with recorded ticks (monotonic, Inv 8/12)"
        );
    }

    /// The periodic snapshot writer is GATED: with `snapshot_interval == 0`
    /// (unbounded) no snapshot is written however far the clock advances.
    #[tokio::test]
    async fn periodic_snapshot_writer_is_gated_on_zero_interval() {
        let (_w, _n, _h, replay_log) = record_one_turn("pong").await;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let mut driver = WorldDriver::open(
            7,
            stub_model(),
            StubModelCaller {
                text: "x".into(),
                calls: AtomicUsize::new(0),
            },
            NoSurfaceDrive,
            replay_log,
            store,
        )
        .await
        .expect("open");

        // Disable periodic snapshots (white-box: `caps` is replayable World state).
        driver.world.resources.caps.snapshot_interval = 0;
        driver.last_snapshot_at = driver.world.clock;

        for _ in 0..3 {
            driver
                .submit(LogicalInput::UserMessage {
                    to: PRIMARY_ENTITY,
                    text: "go".into(),
                })
                .await
                .expect("turn");
        }

        assert!(
            driver
                .store
                .as_ref()
                .expect("the resumed driver owns a store")
                .list_ticks()
                .expect("list snapshot ticks")
                .is_empty(),
            "a zero snapshot_interval writes no snapshots, even as the clock advances"
        );
    }

    /// With a non-zero `snapshot_interval`, the writer captures the World under
    /// the session's `snapshots/` exactly when a tick advance crosses an interval
    /// boundary, and the written snapshot validates (its digest recomputes).
    #[tokio::test]
    async fn periodic_snapshot_writer_writes_when_crossing_the_interval() {
        let (_w, _n, _h, replay_log) = record_one_turn("pong").await;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        let mut driver = WorldDriver::open(
            7,
            stub_model(),
            StubModelCaller {
                text: "x".into(),
                calls: AtomicUsize::new(0),
            },
            NoSurfaceDrive,
            replay_log,
            store,
        )
        .await
        .expect("open");

        // A small interval so one resumed turn crosses a boundary (white-box:
        // `caps` is replayable World state; the resumed clock starts at the
        // recorded frontier, tick 2).
        driver.world.resources.caps.snapshot_interval = 2;
        driver.last_snapshot_at = driver.world.clock;

        driver
            .submit(LogicalInput::UserMessage {
                to: PRIMARY_ENTITY,
                text: "go".into(),
            })
            .await
            .expect("the turn crosses the snapshot boundary");

        let store = driver
            .store
            .as_ref()
            .expect("the resumed driver owns a store");
        assert!(
            !store.list_ticks().expect("list snapshot ticks").is_empty(),
            "crossing the interval writes at least one snapshot under snapshots/"
        );
        let snapshot = store
            .nearest_at_or_before(Tick::MAX)
            .expect("query snapshots")
            .expect("a snapshot was written");
        assert!(
            snapshot.validate(),
            "the written snapshot validates (its digest recomputes)"
        );
    }

    /// A crash-mid-compaction tail is NEVER silently stranded. The recorded log
    /// reconstructs the primary entity in `Compacting { cmd }` (an in-flight
    /// `Command::Compact` written but never `Compacted`); `restore`'s bounded
    /// resume reconciles that `Compact` to a `Redispatch` that `effects::resume`
    /// does NOT log, so `resume_reconciliations` cannot settle it. `open` must NOT
    /// return a driver with an entity stuck `Compacting`: the P0 driver fails
    /// loudly with `Error::World`. (A future re-dispatch implementation that
    /// instead drives the Compact to quiescence — leaving NO entity Compacting —
    /// is equally accepted, so this test pins the SAFETY property, not the policy.)
    #[tokio::test]
    async fn open_never_strands_an_entity_compacting_on_a_crash_tail() {
        use crate::agent::world::inputs::Fingerprint;
        use crate::agent::world::lifecycle::{
            ActorCtx, AppId, EffectKind, IdempotencyKey, LifecycleEvent,
        };
        use crate::agent::world::world::CmdId;

        // The cmd of the compaction call that was in flight when the App crashed.
        let cmd: CmdId = 42;

        // The resume anchor: genesis + folded `SessionStarted`, with the primary
        // entity forced into `Compacting { cmd }` — the in-flight compaction call.
        // Captured as a snapshot so `restore` reconstructs this state directly
        // (without re-running compaction, which depends on context conditions).
        let session = Event {
            origin: Origin::System,
            edge: HUMAN_EDGE,
            at: 0,
            wall: now_wall(),
            input: LogicalInput::SessionStarted {
                seed: 7,
                surface_tools: surface_tool_names().0,
            },
        };
        let (mut world, _no_commands) =
            crate::agent::world::systems::tick(&genesis_world(7, stub_model()), &session);
        world
            .entities
            .get_mut(&PRIMARY_ENTITY)
            .expect("primary entity")
            .activity = Activity::Compacting { cmd };

        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        store
            .write(&capture(world.clone()))
            .expect("write the compacting resume anchor");

        // The recorded log: the `SessionStarted` header (stratum-1, so the log is
        // non-empty and `open` RESUMES) plus the in-flight `Compact` dispatch-intent
        // (stratum-2) with NO `Compacted` result — the truncated mid-effect crash.
        let mut log = MemoryEventLog::new();
        log.append(&session).expect("append the session header");
        log.append_lifecycle(&LifecycleEvent::CommandDispatched {
            at: 0,
            cmd,
            kind: EffectKind::Compact,
            ctx: ActorCtx {
                entity: PRIMARY_ENTITY,
                origin: Origin::Agent,
                edge: HUMAN_EDGE,
            },
            key: IdempotencyKey {
                app_id: AppId::default(),
                tick: 0,
                effect_id: 0,
            },
            fingerprint: Fingerprint(format!("fp-compact-{cmd}")),
        })
        .expect("append the in-flight Compact dispatch-intent");

        let opened = WorldDriver::open(
            7,
            stub_model(),
            ExplodingModelCaller,
            NoSurfaceDrive,
            log,
            store,
        )
        .await;

        match opened {
            Ok(driver) => {
                // The SAFETY property: if `open` returns a driver at all, it must
                // have driven the Compact to quiescence — NO entity left Compacting.
                assert!(
                    !driver
                        .world
                        .entities
                        .values()
                        .any(|c| matches!(c.activity, Activity::Compacting { .. })),
                    "open must never return a driver with an entity stuck Compacting"
                );
            }
            Err(Error::World(message)) => {
                // The P0 choice: fail loudly, naming the stranded compaction.
                assert!(
                    message.to_lowercase().contains("compact"),
                    "the fail-loud error names the stranded compaction: {message}"
                );
            }
            Err(other) => panic!("expected Ok or Error::World, got {other:?}"),
        }
    }

    /// `crosses_snapshot_boundary` fires exactly on the tick advance that enters a
    /// new `interval`-sized bucket: not before the boundary, not twice within a
    /// bucket, never for a non-advancing clock, and never for the unbounded
    /// `interval == 0`.
    #[test]
    fn crosses_snapshot_boundary_detects_interval_crossings() {
        assert!(
            !crosses_snapshot_boundary(0, 63, 64),
            "before the boundary there is no crossing"
        );
        assert!(
            crosses_snapshot_boundary(0, 64, 64),
            "reaching the first boundary crosses"
        );
        assert!(
            !crosses_snapshot_boundary(64, 65, 64),
            "advancing within the same bucket does not cross"
        );
        assert!(
            crosses_snapshot_boundary(63, 130, 64),
            "a jump past two boundaries crosses"
        );
        assert!(
            !crosses_snapshot_boundary(10, 10, 64),
            "a non-advancing clock never crosses"
        );
        assert!(
            !crosses_snapshot_boundary(0, 100, 0),
            "interval 0 (unbounded) never crosses"
        );
    }
}
