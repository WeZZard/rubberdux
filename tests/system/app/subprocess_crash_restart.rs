//! System case: a real worker subprocess is spawned, then crashed, and the
//! host survives and auto-restarts it.
//!
//! Creates an App over a real [`LocalSupervisor`] (which spawns a
//! `--agent` subprocess via `current_exe`, re-entered as a worker by this
//! target's `main`). Subscribes to the App's entry stream and confirms the
//! worker connected by observing the originating turn's first entry. Then it
//! `kill -9`s the worker OS process out from under the supervisor, asserts the
//! host (this process) is still alive and the supervisor still lists the App,
//! sends a fresh message, and confirms a new entry arrives — proving the
//! supervision task respawned a replacement worker that accepted the message.
//!
//! See `docs/app/runtime/worker-lifecycle.md`.

use std::sync::Arc;
use std::time::Duration;

use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::supervisor::{AppSupervisor, CreateAppRequest};
use rubberdux::app::{AppId, BoardPosition, IconSpec};

use crate::live_gate::skip_without_live_llm;

use super::support::{await_entry, kill_worker_process, worker_pids};

pub async fn run() {
    if skip_without_live_llm("subprocess_crash_restart") {
        return;
    }

    let home = tempfile::tempdir().unwrap().into_path();
    let store: Arc<dyn AppStore> =
        Arc::new(FilesystemAppStore::with_apps_dir(home.join("apps")));
    let supervisor = LocalSupervisor::bind(store).await.unwrap();

    let request = CreateAppRequest {
        title: "crash test".into(),
        icon: IconSpec { symbol: "bolt".into(), color: "#FF3B30".into() },
        position: BoardPosition { row: 0, column: 0 },
    };
    let app = supervisor
        .create_app(request, "Say hello and then wait for further instructions.".into())
        .await
        .expect("create_app spawns a worker subprocess");
    let app_id = app.id.clone();

    // Subscribe and confirm the worker connected: the originating turn produces
    // at least one entry forwarded over RPC.
    let mut entries = supervisor.subscribe_entries(&app_id).await.unwrap();
    assert!(
        await_entry(&mut entries, Duration::from_secs(90)).await,
        "the freshly spawned worker must stream an entry from the originating turn"
    );

    // The worker is a real OS subprocess identified by its `--task-id` arg.
    let pids = wait_for_worker(&app_id, Duration::from_secs(10)).await;
    assert!(!pids.is_empty(), "a worker subprocess must exist for the App");

    // Crash every worker process for this App. The host (this process) must
    // survive; the supervision task observes the exit and restarts with backoff.
    for pid in &pids {
        kill_worker_process(*pid);
    }

    // The host is still up: the supervisor still lists the App.
    let listed = supervisor.list().await.unwrap();
    assert!(
        listed.iter().any(|a| a.id == app_id),
        "the host must survive a worker crash and keep the App on the board"
    );

    // A replacement worker is spawned by the supervision task; the App stays
    // addressable and a new message reaches the restarted worker. Re-subscribe
    // after the crash so we observe the replacement worker's stream.
    let mut entries_after = supervisor.subscribe_entries(&app_id).await.unwrap();
    supervisor
        .send_message(&app_id, "Are you back after the restart?".into())
        .await
        .expect("messaging the App restores/uses a live worker");
    assert!(
        await_entry(&mut entries_after, Duration::from_secs(90)).await,
        "after a crash, the auto-restarted worker must accept a new message and stream an entry"
    );
}

/// Poll until at least one worker subprocess for `app_id` exists, or the
/// deadline elapses. The worker takes a moment to spawn, connect, and appear in
/// the process table.
async fn wait_for_worker(app_id: &AppId, within: Duration) -> Vec<u32> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let pids = worker_pids(app_id.as_str());
        if !pids.is_empty() || std::time::Instant::now() >= deadline {
            return pids;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
