//! System case: two Apps, a `peer_send` to a *tombstoned* target, then a
//! restore that delivers the queued message — the decentralized-messaging
//! "restore-then-deliver" path over real subprocess workers and the real
//! broker.
//!
//! Creates a source App and a target App over a real [`LocalSupervisor`].
//! Tombstones the target (so it has no live worker and is addressable only by
//! its inbox), then drives the source worker to `peer_send` to the target's id.
//! The broker, finding the target offline, queues the envelope to the target's
//! `inbox.jsonl` — asserted directly so the test does not depend on the
//! source LLM doing anything observable beyond issuing the send. Restoring the
//! target then drains its inbox as `PeerDeliver` frames, and the target worker
//! reacts (streams an entry) — the deliver half.
//!
//! See `docs/app/peer/decentralized-messaging.md`.

use std::sync::Arc;
use std::time::Duration;

use rubberdux::app::peer::mailbox::Mailbox;
use rubberdux::app::registry::store::{AppStore, FilesystemAppStore};
use rubberdux::app::runtime::local_supervisor::LocalSupervisor;
use rubberdux::app::supervisor::{AppSupervisor, CreateAppRequest};
use rubberdux::app::{AppId, AppStatus, BoardPosition, IconSpec};

use crate::live_gate::skip_without_live_llm;

use super::support::await_entry;

pub async fn run() {
    if skip_without_live_llm("peer_send_tombstoned_target") {
        return;
    }

    let home = tempfile::tempdir().unwrap().into_path();
    let apps_dir = home.join("apps");
    let store: Arc<dyn AppStore> = Arc::new(FilesystemAppStore::with_apps_dir(apps_dir.clone()));
    let supervisor = LocalSupervisor::bind(store.clone()).await.unwrap();

    // The target App: created, then tombstoned so it is offline but addressable.
    let target = supervisor
        .create_app(
            CreateAppRequest {
                title: "peer target".into(),
                icon: IconSpec { symbol: "envelope".into(), color: "#5AC8FA".into() },
                position: BoardPosition { row: 0, column: 1 },
            },
            "You are the target. Acknowledge any peer message you receive.".into(),
        )
        .await
        .unwrap();
    let target_id = target.id.clone();

    // Let the target's originating turn settle, then tombstone it.
    let mut target_entries = supervisor.subscribe_entries(&target_id).await.unwrap();
    let _ = await_entry(&mut target_entries, Duration::from_secs(90)).await;
    supervisor.suspend(&target_id).await.unwrap();
    assert_eq!(
        supervisor.get(&target_id).await.unwrap().unwrap().status,
        AppStatus::Tombstoned,
        "the target must be tombstoned (offline, inbox-addressable) before the send"
    );

    // The source App: instructed to send a peer message to the target's id.
    let source = supervisor
        .create_app(
            CreateAppRequest {
                title: "peer source".into(),
                icon: IconSpec { symbol: "bolt".into(), color: "#34C759".into() },
                position: BoardPosition { row: 0, column: 0 },
            },
            format!(
                "Use the peer_send tool to send the message \"hello neighbor\" to the \
                 peer whose App id is exactly `{target_id}`. Do this now.",
            ),
        )
        .await
        .unwrap();

    // The broker queues the envelope to the offline target's inbox. Poll the
    // inbox until the envelope lands (the relay→queue half).
    let queued = wait_for_inbox_envelope(&apps_dir, &target_id, Duration::from_secs(120)).await;
    assert!(
        queued,
        "a peer_send to a tombstoned target must queue an envelope to its inbox"
    );

    // Restore the target: `ensure_active` drains the inbox as `PeerDeliver`
    // frames, and the target worker reacts — an entry on its stream (the
    // deliver half). Re-subscribe after restore to observe the live worker.
    supervisor.restore(&target_id).await.unwrap();
    let mut delivered_entries = supervisor.subscribe_entries(&target_id).await.unwrap();
    assert!(
        await_entry(&mut delivered_entries, Duration::from_secs(90)).await,
        "restoring the target must deliver the queued peer message and the worker must react"
    );

    // The source App stays addressable throughout (it was never tombstoned).
    assert!(
        supervisor.get(&source.id).await.unwrap().is_some(),
        "the source App remains on the board"
    );
}

/// Poll the target App's `inbox.jsonl` until it holds at least one queued
/// envelope, or the deadline elapses. Reads the mailbox at the App's live
/// directory — where the broker queues for a tombstoned (non-archived) App.
async fn wait_for_inbox_envelope(
    apps_dir: &std::path::Path,
    target_id: &AppId,
    within: Duration,
) -> bool {
    let mailbox_dir = apps_dir.join(target_id.as_str());
    let deadline = std::time::Instant::now() + within;
    loop {
        let mailbox = Mailbox::in_dir(&mailbox_dir);
        if let Ok(meta) = std::fs::metadata(mailbox.path()) {
            if meta.len() > 0 {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
