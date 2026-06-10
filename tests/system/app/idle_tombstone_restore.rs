//! System case: an idle App is tombstoned by the sweeper, then restored — and
//! its on-disk conversation history survives the gap.
//!
//! Sets a 1-second idle window (`RUBBERDUX_APP_IDLE_SECS`) before binding the
//! supervisor so the idle sweeper evicts a quiet App promptly. Creates an App
//! over a real [`LocalSupervisor`], drives one turn (observing an entry so the
//! worker has written history to `session.jsonl`), then waits for the sweeper
//! to tombstone it — observed on the board's `StatusChanged → Tombstoned`
//! event. After the tombstone, the App's on-disk history must still be present.
//! Restoring (a `restore` call, which re-runs the spawn path from the durable
//! session) brings the App back `Active`, and a follow-up message reaches the
//! restored worker.
//!
//! See `docs/app/runtime/worker-lifecycle.md` and `src/app/runtime/lifecycle.rs`.

use std::sync::Arc;
use std::time::Duration;

use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::lifecycle::RUBBERDUX_APP_IDLE_SECS_ENV;
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::supervisor::{AppSupervisor, BoardEvent, CreateAppRequest};
use rubberdux::app::{AppId, AppStatus, BoardPosition, IconSpec};

use crate::live_gate::skip_without_live_llm;

use super::support::await_entry;

pub async fn run() {
    if skip_without_live_llm("idle_tombstone_restore") {
        return;
    }

    // A short idle window so the sweeper evicts the quiet App quickly. Read once
    // by the sweeper at `bind`, so it must be set first. Edition 2024 marks env
    // mutation `unsafe`; this runner is single-threaded at this point.
    unsafe {
        std::env::set_var(RUBBERDUX_APP_IDLE_SECS_ENV, "1");
    }

    let home = tempfile::tempdir().unwrap().into_path();
    let apps_dir = home.join("apps");
    let store: Arc<dyn AppStore> = Arc::new(FilesystemAppStore::with_apps_dir(apps_dir.clone()));
    let supervisor = LocalSupervisor::bind(store.clone()).await.unwrap();

    let mut board = supervisor.subscribe_board();

    let request = CreateAppRequest {
        title: "idle test".into(),
        icon: IconSpec { symbol: "clock".into(), color: "#FFCC00".into() },
        position: BoardPosition { row: 0, column: 0 },
    };
    let app = supervisor
        .create_app(request, "Reply with a single short sentence, then stop.".into())
        .await
        .expect("create_app spawns a worker subprocess");
    let app_id = app.id.clone();

    // Drive the originating turn to completion so history is written to disk.
    let mut entries = supervisor.subscribe_entries(&app_id).await.unwrap();
    assert!(
        await_entry(&mut entries, Duration::from_secs(90)).await,
        "the originating turn must produce an entry (history written to disk)"
    );

    // Wait for the idle sweeper to tombstone the now-quiet App. The sweep cadence
    // is independent of the window, so allow more than one sweep interval.
    let tombstoned = wait_for_tombstone(&mut board, &app_id, Duration::from_secs(75)).await;
    assert!(tombstoned, "the idle sweeper must tombstone the quiet App");
    assert_eq!(
        supervisor.get(&app_id).await.unwrap().unwrap().status,
        AppStatus::Tombstoned,
        "a swept App reads Tombstoned"
    );

    // History is preserved on disk across the tombstone: the App's session
    // directory still holds a non-empty `session.jsonl`.
    assert!(
        app_has_persisted_history(&apps_dir, &app_id),
        "the tombstoned App's on-disk conversation history must survive"
    );

    // Restore continues from the durable session and brings the App Active.
    supervisor.restore(&app_id).await.unwrap();
    assert_eq!(
        supervisor.get(&app_id).await.unwrap().unwrap().status,
        AppStatus::Active,
        "restore re-activates the App from on-disk history"
    );

    // The restored worker accepts a follow-up message and streams an entry.
    let mut entries_after = supervisor.subscribe_entries(&app_id).await.unwrap();
    supervisor
        .send_message(&app_id, "Continue from where we left off.".into())
        .await
        .unwrap();
    assert!(
        await_entry(&mut entries_after, Duration::from_secs(90)).await,
        "the restored worker must accept a follow-up message and stream an entry"
    );
}

/// Drain board events until the App reports `Tombstoned`, or the deadline
/// elapses. Returns `true` on observing the tombstone.
async fn wait_for_tombstone(
    board: &mut tokio::sync::broadcast::Receiver<BoardEvent>,
    app_id: &AppId,
    within: Duration,
) -> bool {
    tokio::time::timeout(within, async {
        loop {
            match board.recv().await {
                Ok(BoardEvent::StatusChanged { id, status })
                    if &id == app_id && status == AppStatus::Tombstoned =>
                {
                    return true;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Whether the App's on-disk session directory holds a non-empty
/// `session.jsonl`. The worker roots its `SessionManager` under
/// `<app_dir>/sessions/<session_id>/agent_main/session.jsonl`.
fn app_has_persisted_history(apps_dir: &std::path::Path, app_id: &AppId) -> bool {
    let sessions = apps_dir.join(app_id.as_str()).join("sessions");
    let Ok(read) = std::fs::read_dir(&sessions) else {
        return false;
    };
    for session in read.flatten() {
        let jsonl = session.path().join("agent_main").join("session.jsonl");
        if std::fs::metadata(&jsonl).map(|m| m.len() > 0).unwrap_or(false) {
            return true;
        }
    }
    false
}
