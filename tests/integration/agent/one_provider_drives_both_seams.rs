//! V8.1 — one selected provider drives BOTH model-call seams.
//!
//! The plan locks the whole system to ONE config-selected [`ModelApi`]
//! (`docs/provider/`, Verification V8). This file is the wiring proof: a SINGLE
//! counting stub is handed, as that same instance, to BOTH model-call runtimes —
//!
//! - the HOST agent loop (`turn_driver`, via [`AgentLoopHarness`], which holds the
//!   provider as `Arc<dyn ModelApi>` exactly as the production `AgentLoopBuilder`
//!   does), and
//! - the per-App WORLD driver ([`drive_live`], which takes the provider as
//!   `&dyn ModelApi`).
//!
//! Driving one turn through each path bumps the stub's SHARED call counter, so the
//! count climbs 0 → 1 (host) → 2 (world): both runtimes reached the SAME provider
//! instance, which is precisely "the system is locked to one provider across both
//! seams" (V8.1). It is offline and deterministic — it makes no network call and
//! needs no credentials, so it runs on every developer machine.
//!
//! See `docs/provider/` (config-locked selection) and Verification V8 / V8.1.

use std::sync::Arc;
use std::time::Duration;

use rubberdux::agent::world::effects::{
    Command, CommandKey, ResultStamp, SurfaceDriver, ToolSet, UnattachedPeerSender, drive_live,
};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::history::{Block, History, Msg, Role};
use rubberdux::agent::world::inputs::LogicalInput;
use rubberdux::agent::world::world::{CmdId, Effort, ModelConfig, Resources, World};
use rubberdux::error::Error;
use rubberdux::provider::ModelApi;
use rubberdux::tool::ToolRegistry;

use crate::support::agent_loop_harness::AgentLoopHarness;
use crate::support::artifact;
use crate::support::model_api_stub::CountingModelApi;

/// A no-op surface-drive sink: this world turn drives only a `CallModel` (no
/// `set_value` `RunTool`), so the driver is never reached — it stands in for the
/// injected sink so [`drive_live`] type-checks.
struct NoSurfaceDrive;
impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        Ok(())
    }
}

/// The world-default `ModelConfig` for the offline world turn. Its alias is inert:
/// the counting stub answers without inspecting it.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "counting-stub".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// A trivial `World` whose perceived surface view is empty — the only thing a
/// `CallModel`-only [`drive_live`] reads from `&World`.
fn surfaceless_world() -> World {
    World::new(0, Resources::new(7, offline_model()))
}

/// One `CallModel` for entity 0 carrying `text` as its sole user message — the unit
/// of work the world live driver dispatches through the provider.
fn call_model(cmd: CmdId, text: &str) -> Command {
    Command::CallModel {
        cmd,
        entity: 0,
        messages: History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text { text: text.into() }],
        }]),
        tools: ToolSet::default(),
        params: offline_model(),
        key: CommandKey,
    }
}

/// **V8.1** A single counting [`ModelApi`] is selected as the provider and handed
/// — the SAME instance — to BOTH the host agent loop and the per-App world driver.
/// One turn through each path increments the stub's shared counter, so its final
/// value (2) proves both runtimes routed through the one selected provider.
#[tokio::test(flavor = "multi_thread")]
async fn one_provider_instance_drives_both_host_and_world_seams() {
    // ONE selected provider, shared across both seams. `Arc<CountingModelApi>`
    // coerces to the `Arc<dyn ModelApi>` the host loop holds and to the
    // `&dyn ModelApi` the world driver takes — both views of THIS allocation.
    let stub: Arc<CountingModelApi> = Arc::new(CountingModelApi::new());
    assert_eq!(stub.count(), 0, "the stub starts with zero turns served");

    // --- HOST seam: one agent-loop turn through `turn_driver` ------------------
    // The harness builds `AgentLoop` with the provider as `Arc<dyn ModelApi>`,
    // exactly as production `AgentLoopBuilder` does. An empty registry keeps the
    // turn tool-free; the stub's `EndTurn` settles it in a single model call.
    let artifact_dir = artifact::artifact_dir("v8_1_one_provider_drives_both_seams");
    let session_path = artifact_dir.join("transcript.jsonl");
    let harness = AgentLoopHarness::new_with_registry(
        "You are a test assistant.",
        session_path.clone(),
        stub.clone(),
        Arc::new(ToolRegistry::new()),
    )
    .await;

    let exchange = harness
        .send_message("drive one host turn", Duration::from_secs(30))
        .await;

    let narration = artifact::narrate_session(&session_path);
    artifact::write_narration(&session_path, &narration);

    assert!(
        exchange.failure_reason.is_none(),
        "the host agent-loop turn must complete: {:?}",
        exchange.failure_reason
    );
    assert!(
        exchange.outputs.iter().any(|o| o.is_final),
        "the host agent-loop turn must produce a final assistant message"
    );

    let after_host = stub.count();
    assert_eq!(
        after_host, 1,
        "the host seam must drive exactly one turn through the selected provider"
    );

    // --- WORLD seam: one `drive_live` turn through the SAME instance -----------
    // `drive_live` takes the provider as `&dyn ModelApi`; `stub.as_ref()` is the
    // SAME instance the host loop holds, so its turn bumps the SAME counter.
    let stamp = ResultStamp {
        edge: 0,
        app_edge: 1,
        at: 2,
        wall: None,
    };
    let mut log = MemoryEventLog::new();
    let results = drive_live(
        &[call_model(1, "drive one world turn")],
        stamp,
        &surfaceless_world(),
        stub.as_ref(),
        &NoSurfaceDrive,
        &UnattachedPeerSender,
        &mut log,
    )
    .await
    .expect("the world live driver dispatches the CallModel through the provider");

    assert_eq!(results.len(), 1, "one CallModel yields one result event");
    assert!(
        matches!(results[0].input, LogicalInput::ModelResponded { .. }),
        "the world turn records a ModelResponded from the provider call"
    );

    let after_world = stub.count();
    assert!(
        after_world > after_host,
        "the world seam must also reach the SAME provider instance (counter advanced)"
    );

    // The two seams shared ONE provider instance: its counter holds the total
    // (1 host + 1 world), proving the system locked to one provider across both
    // runtimes (V8.1).
    assert_eq!(
        after_world, 2,
        "one host turn + one world turn = two turns served by the single selected provider"
    );
}
