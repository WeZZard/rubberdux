//! The host-side handle to one native App worker process.
//!
//! A [`WorkerHandle`] is the host's view of a single `rubberduxd --agent`
//! subprocess spawned by [`crate::app::runtime::local_supervisor::LocalSupervisor`].
//! It owns the per-App observable streams the board renders from (entry and
//! trajectory broadcasts), the outbound channel used to push
//! [`HostToAgent`](crate::protocol::HostToAgent) frames into the worker, the
//! interactions the worker is awaiting answers for, and the cancellation token
//! that tears the worker's supervision task down on suspend/archive.
//!
//! The handle is deliberately transport-agnostic about *which* socket is live:
//! a crashed worker is reconnected by the supervision task, which swaps in the
//! new socket's writer behind the same outbound channel, so the handle (and the
//! board subscribers reading from it) survive restarts. The lifecycle rationale
//! is in `docs/app/runtime/worker-lifecycle.md`.

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::interaction::AgentInteraction;
use crate::agent::runtime::port::EntryNotification;
use crate::error::Error;
use crate::protocol::HostToAgent;
use crate::trajectory::TrajectoryEvent;

/// Handle to one running native App worker: the channels and tokens the
/// supervisor needs to message it, observe it, and shut it down. Held only while
/// the App is `Active`; dropped on suspend/archive so the supervision task that
/// owns the child process and its RPC pump exits.
///
/// Mirrors the surface of `crate::app::supervisor::AppRuntime` so a
/// [`LocalSupervisor`](crate::app::runtime::local_supervisor::LocalSupervisor)
/// can stand in for `MemorySupervisor` behind the `AppSupervisor` seam.
pub struct WorkerHandle {
    /// Outbound frames to the worker. The supervision task drains this and
    /// writes each frame to the *current* connection's writer, so the queue
    /// survives a reconnect after a crash.
    outbound_tx: mpsc::Sender<HostToAgent>,
    /// Re-broadcasts the worker's `EntryNotification` frames to board
    /// subscribers. Kept alive for the handle's lifetime so late subscribers can
    /// attach even between reconnects.
    entry_tx: broadcast::Sender<EntryNotification>,
    /// Broadcasts the worker's trajectory events to board subscribers. The
    /// native RPC protocol does not yet carry a trajectory frame, so this
    /// channel is wired to the same surface as `MemorySupervisor` and stays
    /// quiet until such a frame lands; keeping it preserves the seam.
    trajectory_tx: broadcast::Sender<TrajectoryEvent>,
    /// Interactions the worker raised and is awaiting an answer for.
    pending_interactions: Vec<AgentInteraction>,
    /// Cancels the worker's supervision task (which owns the child process and
    /// the RPC pump) on suspend/archive.
    cancel: CancellationToken,
}

impl WorkerHandle {
    /// Assemble a handle from the channels and token the supervision task wires
    /// up. The supervision task keeps the receiving ends; the handle keeps the
    /// sending/broadcasting ends the supervisor's `&self` methods read from.
    pub fn new(
        outbound_tx: mpsc::Sender<HostToAgent>,
        entry_tx: broadcast::Sender<EntryNotification>,
        trajectory_tx: broadcast::Sender<TrajectoryEvent>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            outbound_tx,
            entry_tx,
            trajectory_tx,
            pending_interactions: Vec::new(),
            cancel,
        }
    }

    /// Queue a frame for delivery to the worker. Errors if the supervision task
    /// has exited (e.g. the worker was suspended), mirroring the "no active
    /// worker" failure the `AppSupervisor` trait specifies.
    pub async fn send_frame(&self, frame: HostToAgent) -> Result<(), Error> {
        self.outbound_tx
            .send(frame)
            .await
            .map_err(|_| Error::App("worker is not running".to_string()))
    }

    /// Subscribe to the worker's entry-notification stream.
    pub fn subscribe_entries(&self) -> broadcast::Receiver<EntryNotification> {
        self.entry_tx.subscribe()
    }

    /// Subscribe to the worker's trajectory-event stream.
    pub fn subscribe_trajectory(&self) -> broadcast::Receiver<TrajectoryEvent> {
        self.trajectory_tx.subscribe()
    }

    /// The interactions the worker has raised and is awaiting a response for.
    pub fn pending_interactions(&self) -> Vec<AgentInteraction> {
        self.pending_interactions.clone()
    }

    /// Whether any interaction is awaiting a user answer. Read by the supervisor
    /// to keep the App's lifecycle pending-interaction flag in sync, so the idle
    /// sweeper never evicts an App that is blocked on a user response.
    pub fn has_pending_interaction(&self) -> bool {
        !self.pending_interactions.is_empty()
    }

    /// Record an interaction the worker just raised, replacing any prior entry
    /// with the same `request_id` so a re-raised interaction is not duplicated.
    /// Called from the RPC pump when an [`AgentToHost::Interaction`] frame
    /// arrives.
    pub fn add_interaction(&mut self, interaction: AgentInteraction) {
        let request_id = interaction.request_id().to_string();
        self.pending_interactions
            .retain(|existing| existing.request_id() != request_id);
        self.pending_interactions.push(interaction);
    }

    /// Replace the pending interactions wholesale. Used on restore to re-attach
    /// the interactions a tombstoned worker had been awaiting answers for, taken
    /// from its `resume.json`, before the record is cleared.
    pub fn set_pending_interactions(&mut self, interactions: Vec<AgentInteraction>) {
        self.pending_interactions = interactions;
    }

    /// Drop the pending interaction matching `request_id`, called when its answer
    /// has been routed back to the worker.
    pub fn clear_interaction(&mut self, request_id: &str) {
        self.pending_interactions
            .retain(|interaction| interaction.request_id() != request_id);
    }

    /// Stop the worker: cancel its supervision task so the child process is
    /// killed and the RPC pump exits. Idempotent.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_capacity() -> usize {
        16
    }

    fn sample_handle() -> (WorkerHandle, mpsc::Receiver<HostToAgent>) {
        let (outbound_tx, outbound_rx) = mpsc::channel(channel_capacity());
        let (entry_tx, _) = broadcast::channel(channel_capacity());
        let (trajectory_tx, _) = broadcast::channel(channel_capacity());
        let handle = WorkerHandle::new(
            outbound_tx,
            entry_tx,
            trajectory_tx,
            CancellationToken::new(),
        );
        (handle, outbound_rx)
    }

    #[tokio::test]
    async fn send_frame_reaches_the_supervision_task() {
        let (handle, mut outbound_rx) = sample_handle();
        handle
            .send_frame(HostToAgent::UserMessage {
                text: "hello".into(),
                telegram_message_id: None,
            })
            .await
            .unwrap();
        match outbound_rx.recv().await.unwrap() {
            HostToAgent::UserMessage { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_frame_errors_when_supervision_task_gone() {
        let (handle, outbound_rx) = sample_handle();
        // Dropping the receiver models the supervision task having exited.
        drop(outbound_rx);
        assert!(matches!(
            handle
                .send_frame(HostToAgent::Shutdown)
                .await,
            Err(Error::App(_))
        ));
    }

    #[tokio::test]
    async fn subscriptions_observe_broadcast_frames() {
        let (handle, _outbound_rx) = sample_handle();
        let mut entries = handle.subscribe_entries();
        // The handle exposes the sender to the supervision task; here we reach it
        // through a fresh subscription path by re-broadcasting on a clone.
        handle
            .entry_tx
            .send(EntryNotification {
                entry: crate::agent::entry::Entry {
                    id: 1,
                    parent_id: None,
                    message: crate::provider::kimi_for_coding::Message::User {
                        content: crate::provider::kimi_for_coding::UserContent::Text("x".into()),
                    },
                    origin: crate::agent::entry::EntryOrigin::User {
                        channel: "board".into(),
                    },
                    channel_metadata: None,
                },
                is_final: true,
            })
            .unwrap();
        assert_eq!(entries.recv().await.unwrap().entry.id, 1);
    }

    #[test]
    fn shutdown_cancels_the_token() {
        let (handle, _outbound_rx) = sample_handle();
        assert!(!handle.cancel.is_cancelled());
        handle.shutdown();
        assert!(handle.cancel.is_cancelled());
    }

    #[test]
    fn add_interaction_records_and_replaces_by_request_id() {
        let (mut handle, _outbound_rx) = sample_handle();
        assert!(!handle.has_pending_interaction());
        let raise = |id: &str, prompt: &str| AgentInteraction::Approval {
            request_id: id.into(),
            app_id: "a".into(),
            flavor: crate::agent::interaction::ApprovalFlavor::Permission,
            prompt: prompt.into(),
        };
        handle.add_interaction(raise("r1", "first"));
        assert!(handle.has_pending_interaction());
        assert_eq!(handle.pending_interactions().len(), 1);
        // Re-raising the same request id replaces rather than duplicates.
        handle.add_interaction(raise("r1", "second"));
        let pending = handle.pending_interactions();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id(), "r1");
    }

    #[test]
    fn set_pending_interactions_replaces_wholesale() {
        let (mut handle, _outbound_rx) = sample_handle();
        handle.add_interaction(AgentInteraction::Approval {
            request_id: "old".into(),
            app_id: "a".into(),
            flavor: crate::agent::interaction::ApprovalFlavor::Permission,
            prompt: "?".into(),
        });
        handle.set_pending_interactions(vec![AgentInteraction::Approval {
            request_id: "restored".into(),
            app_id: "a".into(),
            flavor: crate::agent::interaction::ApprovalFlavor::Permission,
            prompt: "?".into(),
        }]);
        let pending = handle.pending_interactions();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id(), "restored");
    }

    #[test]
    fn clear_interaction_drops_only_the_matching_request() {
        let (mut handle, _outbound_rx) = sample_handle();
        handle.pending_interactions = vec![
            AgentInteraction::Approval {
                request_id: "keep".into(),
                app_id: "a".into(),
                flavor: crate::agent::interaction::ApprovalFlavor::Permission,
                prompt: "?".into(),
            },
            AgentInteraction::Approval {
                request_id: "drop".into(),
                app_id: "a".into(),
                flavor: crate::agent::interaction::ApprovalFlavor::Permission,
                prompt: "?".into(),
            },
        ];
        handle.clear_interaction("drop");
        let remaining = handle.pending_interactions();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].request_id(), "keep");
    }
}
