//! Verifies the host's surface-relay routing: the [`SurfaceRouter`] relays a
//! worker's outbound `AgentToHost::SurfaceDrive` to the App's registered macOS
//! client (as `HostToAgent::SurfaceDrive`) and a client's inbound
//! `AgentToHost::SurfaceObservation` to the App's registered worker seam — both
//! keyed by App identity, NOT by registration/accept order. This is the offline
//! proof of the W4 host-wiring relay; the full live path is proven by the live V
//! system test. See `docs/agent/world/ecs-runtime.md` (Theme 2a/2b; VC-P.1).

use rubberdux::agent::world::surface::{
    Hash, IdempotencyKey, SurfaceId, SurfaceOp, Viewport, WindowState,
};
use rubberdux::host::SurfaceRouter;
use rubberdux::protocol::{AgentToHost, HostToAgent, SurfaceObserved};

/// Build a representative observed-surface snapshot keyed to `surface`.
fn observed(surface: SurfaceId, digest: &str) -> SurfaceObserved {
    SurfaceObserved {
        surface,
        version: 0,
        ax_digest: Hash(digest.into()),
        focus: None,
        selection: None,
        viewport: Viewport(String::new()),
        window: WindowState(String::new()),
        cursor: None,
    }
}

/// [Verifies VC-P.1] Two Apps registered out of accept order route by App
/// identity. The host repackages a worker's `AgentToHost::SurfaceDrive` as the
/// client-facing `HostToAgent::SurfaceDrive` and routes it to the matching App's
/// client; it routes an inbound client observation to the matching App's worker
/// seam. Neither leg leaks to the other App, regardless of registration order.
#[tokio::test]
async fn surface_router_relays_drive_and_observation_by_app_identity() {
    let router = SurfaceRouter::new();

    // Register App-B before App-A — out of accept/alphabetical order — so a pass
    // proves routing is keyed by identity, not registration sequence.
    let (drive_tx_b, mut drive_rx_b) = tokio::sync::mpsc::channel::<HostToAgent>(4);
    let (worker_tx_b, mut worker_rx_b) = tokio::sync::mpsc::channel::<AgentToHost>(4);
    router.register_client("app-b".into(), drive_tx_b).await;
    router.register_worker("app-b".into(), worker_tx_b).await;

    let (drive_tx_a, mut drive_rx_a) = tokio::sync::mpsc::channel::<HostToAgent>(4);
    let (worker_tx_a, mut worker_rx_a) = tokio::sync::mpsc::channel::<AgentToHost>(4);
    router.register_client("app-a".into(), drive_tx_a).await;
    router.register_worker("app-a".into(), worker_tx_a).await;

    // -- Drive leg: worker(A) → host → client(A) ------------------------------
    // The worker emits its outbound drive as `AgentToHost::SurfaceDrive`; the host
    // repackages it as the client-facing `HostToAgent::SurfaceDrive` and routes it
    // to App-A's registered client (exactly what `LocalSupervisor::pump` does).
    let worker_drive = AgentToHost::SurfaceDrive {
        ops: vec![SurfaceOp::SetValue {
            surface: 1,
            element: 2,
            value: serde_json::json!("from agent A"),
            base_version: None,
        }],
        cmd: 7,
        key: IdempotencyKey("cmd-7".into()),
    };
    let AgentToHost::SurfaceDrive { ops, cmd, key } = worker_drive else {
        panic!("constructed an AgentToHost::SurfaceDrive");
    };
    let client_drive = HostToAgent::SurfaceDrive { ops, cmd, key };
    assert!(
        router.route_drive("app-a", client_drive).await,
        "route_drive must report delivery to App-A's client"
    );
    match drive_rx_a.recv().await {
        Some(HostToAgent::SurfaceDrive { cmd, key, ops }) => {
            assert_eq!(cmd, 7, "the relayed drive must carry the worker's cmd");
            assert_eq!(key, IdempotencyKey("cmd-7".into()));
            assert_eq!(ops.len(), 1, "the relayed drive must carry the ops");
        }
        other => panic!("expected a SurfaceDrive at App-A's client, got {other:?}"),
    }
    assert!(
        drive_rx_b.try_recv().is_err(),
        "App-A's drive must NOT reach App-B's client"
    );

    // -- Observation leg: client(A) → host → worker(A) ------------------------
    // The client reports an observation as `AgentToHost::SurfaceObservation`; the
    // host routes it to App-A's worker seam by identity.
    let client_obs = AgentToHost::SurfaceObservation {
        observed: observed(1, "digest-a"),
    };
    assert!(
        router.route_inbound("app-a", client_obs).await,
        "route_inbound must report delivery to App-A's worker seam"
    );
    match worker_rx_a.recv().await {
        Some(AgentToHost::SurfaceObservation { observed }) => {
            assert_eq!(observed.surface, 1);
            assert_eq!(observed.ax_digest, Hash("digest-a".into()));
        }
        other => panic!("expected a SurfaceObservation at App-A's worker, got {other:?}"),
    }
    assert!(
        worker_rx_b.try_recv().is_err(),
        "App-A's observation must NOT reach App-B's worker seam"
    );

    // The symmetric routes for App-B must still resolve to App-B's own sinks,
    // confirming both identities coexist in the one router.
    let b_obs = AgentToHost::SurfaceObservation {
        observed: observed(9, "digest-b"),
    };
    assert!(router.route_inbound("app-b", b_obs).await);
    match worker_rx_b.recv().await {
        Some(AgentToHost::SurfaceObservation { observed }) => {
            assert_eq!(observed.surface, 9);
            assert_eq!(observed.ax_digest, Hash("digest-b".into()));
        }
        other => panic!("expected a SurfaceObservation at App-B's worker, got {other:?}"),
    }
    assert!(
        worker_rx_a.try_recv().is_err(),
        "App-B's observation must NOT reach App-A's worker seam"
    );
}

/// A drive or observation for an App with no registration is reported undelivered
/// rather than misrouted — the relay never guesses a target.
#[tokio::test]
async fn surface_router_reports_undelivered_for_unregistered_app() {
    let router = SurfaceRouter::new();
    let drive = HostToAgent::SurfaceDrive {
        ops: vec![],
        cmd: 1,
        key: IdempotencyKey("cmd-1".into()),
    };
    assert!(
        !router.route_drive("nobody", drive).await,
        "an unregistered App's drive must report no delivery"
    );
    let obs = AgentToHost::SurfaceObservation {
        observed: observed(1, "d"),
    };
    assert!(
        !router.route_inbound("nobody", obs).await,
        "an unregistered App's observation must report no delivery"
    );
}
