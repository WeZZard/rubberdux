use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

use super::event::{TrajectoryEvent, TrajectoryEventDraft};

pub trait TrajectoryRecorder: Send + Sync {
    fn record(&self, draft: TrajectoryEventDraft);
}

pub type SharedTrajectoryRecorder = std::sync::Arc<dyn TrajectoryRecorder>;

#[derive(Default)]
pub struct NoopTrajectoryRecorder;

impl TrajectoryRecorder for NoopTrajectoryRecorder {
    fn record(&self, _draft: TrajectoryEventDraft) {}
}

pub struct MemoryTrajectoryRecorder {
    next_seq: AtomicU64,
    events: Mutex<Vec<TrajectoryEvent>>,
}

impl MemoryTrajectoryRecorder {
    pub fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<TrajectoryEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl TrajectoryRecorder for MemoryTrajectoryRecorder {
    fn record(&self, draft: TrajectoryEventDraft) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.events
            .lock()
            .unwrap()
            .push(TrajectoryEvent::from_draft(seq, draft));
    }
}

pub struct FilesystemTrajectoryRecorder {
    next_seq: AtomicU64,
    tx: mpsc::UnboundedSender<TrajectoryEvent>,
}

impl FilesystemTrajectoryRecorder {
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<TrajectoryEvent>();

        tokio::spawn(async move {
            if let Some(parent) = path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    log::warn!("Failed to create trajectory event directory: {}", e);
                    return;
                }
            }

            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(file) => file,
                Err(e) => {
                    log::warn!("Failed to open trajectory event log {:?}: {}", path, e);
                    return;
                }
            };

            while let Some(event) = rx.recv().await {
                let json = match serde_json::to_string(&event) {
                    Ok(json) => json,
                    Err(e) => {
                        log::warn!("Failed to serialize trajectory event: {}", e);
                        continue;
                    }
                };

                if let Err(e) =
                    tokio::io::AsyncWriteExt::write_all(&mut file, json.as_bytes()).await
                {
                    log::warn!("Failed to write trajectory event: {}", e);
                    continue;
                }
                if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, b"\n").await {
                    log::warn!("Failed to terminate trajectory event line: {}", e);
                    continue;
                }
            }
        });

        Self {
            next_seq: AtomicU64::new(0),
            tx,
        }
    }
}

impl TrajectoryRecorder for FilesystemTrajectoryRecorder {
    fn record(&self, draft: TrajectoryEventDraft) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let event = TrajectoryEvent::from_draft(seq, draft);
        if self.tx.send(event).is_err() {
            log::warn!("Trajectory event writer is closed");
        }
    }
}

pub struct BroadcastTrajectoryRecorder {
    inner: SharedTrajectoryRecorder,
    tx: tokio::sync::broadcast::Sender<TrajectoryEvent>,
    next_seq: AtomicU64,
}

impl BroadcastTrajectoryRecorder {
    pub fn new(
        inner: SharedTrajectoryRecorder,
        tx: tokio::sync::broadcast::Sender<TrajectoryEvent>,
    ) -> Self {
        Self {
            inner,
            tx,
            next_seq: AtomicU64::new(0),
        }
    }
}

impl TrajectoryRecorder for BroadcastTrajectoryRecorder {
    fn record(&self, draft: TrajectoryEventDraft) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let inner_draft = draft.clone();
        let event = TrajectoryEvent::from_draft(seq, draft);
        self.inner.record(inner_draft);
        let _ = self.tx.send(event);
    }
}

pub fn noop_recorder() -> SharedTrajectoryRecorder {
    std::sync::Arc::new(NoopTrajectoryRecorder)
}

pub fn filesystem_recorder(path: PathBuf) -> SharedTrajectoryRecorder {
    std::sync::Arc::new(FilesystemTrajectoryRecorder::spawn(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_recorder_assigns_sequence_numbers() {
        let recorder = MemoryTrajectoryRecorder::new();

        recorder.record(TrajectoryEventDraft::new("a", "test", "main"));
        recorder.record(TrajectoryEventDraft::new("b", "test", "main"));

        let events = recorder.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);
        assert_eq!(events[0].event_id, "evt_0");
        assert_eq!(events[1].event_id, "evt_1");
    }

    #[test]
    fn test_broadcast_recorder_delegates_to_inner() {
        let inner = std::sync::Arc::new(MemoryTrajectoryRecorder::new());
        let shared: SharedTrajectoryRecorder = inner.clone();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let recorder = BroadcastTrajectoryRecorder::new(shared, tx);

        recorder.record(TrajectoryEventDraft::new("test.event", "test", "main"));

        let events = inner.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "test.event");
    }

    #[test]
    fn test_broadcast_recorder_broadcasts() {
        let inner = std::sync::Arc::new(MemoryTrajectoryRecorder::new());
        let shared: SharedTrajectoryRecorder = inner.clone();
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let recorder = BroadcastTrajectoryRecorder::new(shared, tx);

        recorder.record(TrajectoryEventDraft::new("broadcast.test", "test", "main"));

        let event = rx.try_recv().expect("should receive broadcast event");
        assert_eq!(event.event_type, "broadcast.test");
        assert_eq!(event.seq, 0);
    }
}
