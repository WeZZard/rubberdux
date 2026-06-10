use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};

use crate::agent::entry::Entry;
use crate::agent::runtime::port::{EntryNotification, InputPort};
use crate::app::supervisor::{AppSupervisor, BoardEvent};
use crate::gateway::apps::DynAppSupervisor;
use crate::gateway::apps_stream::InteractionEvent;
use crate::provider::moonshot::MoonshotClient;
use crate::trajectory::TrajectoryEvent;

pub struct GatewayState {
    pub entries: RwLock<Vec<Entry>>,
    pub system_prompt: String,
    pub identity_prompt: String,
    pub soul_prompt: String,
    pub entry_tx: broadcast::Sender<EntryNotification>,
    pub trajectory_tx: broadcast::Sender<TrajectoryEvent>,
    pub input_port: InputPort,
    pub events_path: Option<std::path::PathBuf>,
    /// The App supervisor backing the multi-App board REST surface. Present only
    /// when the gateway is constructed via [`GatewayState::with_apps`]; the
    /// existing single-agent entry points leave it `None`. Held as the
    /// object-safe [`DynAppSupervisor`] view because the source `AppSupervisor`
    /// trait uses bare `async fn` and is not itself `dyn`-compatible. See
    /// `docs/gateway/apps.md`.
    pub supervisor: Option<Arc<dyn DynAppSupervisor>>,
    /// The Moonshot client used to derive an App's identity (title + icon) as a
    /// background task when an App is created. Present only alongside
    /// `supervisor`.
    pub identity_client: Option<Arc<MoonshotClient>>,
    /// Gateway-owned fan-out of App interaction lifecycle events
    /// (`Raised`/`Resolved`). The `AppSupervisor` trait exposes interactions
    /// only as a snapshot poll and an answer path, with no live "raised" event;
    /// the board WebSocket surface derives its `badge` and per-App
    /// `interaction_raised`/`resolved` streams from this channel instead of
    /// widening the supervisor trait. See `docs/gateway/apps_stream.md`.
    pub interaction_tx: broadcast::Sender<InteractionEvent>,
    /// Gateway-owned re-broadcast of the supervisor's board lifecycle events.
    /// The object-safe `DynAppSupervisor` view the gateway stores does not
    /// expose `subscribe_board`, so `with_apps` attaches one supervisor board
    /// subscription and forwards every event here; the board WebSocket fans this
    /// out to every connected client. Empty (no forwarder) for the single-agent
    /// constructors. See `docs/gateway/apps_stream.md`.
    pub board_tx: broadcast::Sender<BoardEvent>,
}

impl GatewayState {
    pub fn new(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
        input_port: InputPort,
    ) -> Self {
        let (entry_tx, _) = broadcast::channel(256);
        let (trajectory_tx, _) = broadcast::channel(256);
        let (interaction_tx, _) = broadcast::channel(256);
        Self {
            entries: RwLock::new(Vec::new()),
            system_prompt,
            identity_prompt,
            soul_prompt,
            entry_tx,
            trajectory_tx,
            input_port,
            events_path: None,
            supervisor: None,
            identity_client: None,
            interaction_tx,
            board_tx: broadcast::channel(256).0,
        }
    }

    pub fn with_trajectory_tx(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
        trajectory_tx: broadcast::Sender<TrajectoryEvent>,
        input_port: InputPort,
        events_path: Option<std::path::PathBuf>,
    ) -> Self {
        let (entry_tx, _) = broadcast::channel(256);
        let (interaction_tx, _) = broadcast::channel(256);
        Self {
            entries: RwLock::new(Vec::new()),
            system_prompt,
            identity_prompt,
            soul_prompt,
            entry_tx,
            trajectory_tx,
            input_port,
            events_path,
            supervisor: None,
            identity_client: None,
            interaction_tx,
            board_tx: broadcast::channel(256).0,
        }
    }

    /// Construct a gateway state wired for the multi-App board: it carries the
    /// [`AppSupervisor`] the board REST surface drives and the
    /// [`MoonshotClient`] used to derive App identities in the background. The
    /// single-agent entry endpoints remain available; their fields default to
    /// empty so one process can serve both surfaces. See `docs/gateway/apps.md`.
    pub fn with_apps<S>(
        system_prompt: String,
        identity_prompt: String,
        soul_prompt: String,
        input_port: InputPort,
        supervisor: Arc<S>,
        identity_client: Arc<MoonshotClient>,
    ) -> Self
    where
        S: AppSupervisor + 'static,
    {
        let (entry_tx, _) = broadcast::channel(256);
        let (trajectory_tx, _) = broadcast::channel(256);
        let (interaction_tx, _) = broadcast::channel(256);
        let (board_tx, _) = broadcast::channel(256);

        // The object-safe `DynAppSupervisor` view stored below does not expose
        // `subscribe_board`, so capture one board subscription off the concrete
        // supervisor here and forward every event into the gateway's own board
        // fan-out, which the board WebSocket renders from. See
        // `docs/gateway/apps_stream.md`.
        let mut source_board = supervisor.subscribe_board();
        let board_forward = board_tx.clone();
        tokio::spawn(async move {
            loop {
                match source_board.recv().await {
                    Ok(event) => {
                        // No active board clients right now is not an error.
                        let _ = board_forward.send(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Self {
            entries: RwLock::new(Vec::new()),
            system_prompt,
            identity_prompt,
            soul_prompt,
            entry_tx,
            trajectory_tx,
            input_port,
            events_path: None,
            // Coerce the concrete supervisor into the object-safe view the
            // gateway stores. See `docs/gateway/apps.md`.
            supervisor: Some(supervisor as Arc<dyn DynAppSupervisor>),
            identity_client: Some(identity_client),
            interaction_tx,
            board_tx,
        }
    }

    /// Publish an App interaction lifecycle event onto the gateway's
    /// interaction fan-out. The board and per-App interaction WebSocket
    /// handlers render their `badge` and `interaction_raised`/`resolved`
    /// streams from this channel. A send error means there are no current
    /// subscribers, which is not a failure. See `docs/gateway/apps_stream.md`.
    pub fn publish_interaction(&self, event: InteractionEvent) {
        let _ = self.interaction_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::entry::EntryOrigin;
    use crate::agent::runtime::port::LoopEvent;
    use crate::provider::moonshot::{Message, UserContent};

    fn dummy_input_port() -> InputPort {
        let (tx, _rx) = tokio::sync::mpsc::channel::<LoopEvent>(8);
        InputPort::new(tx)
    }

    #[test]
    fn test_new_creates_empty_state() {
        let state = GatewayState::new(
            "system".into(),
            "identity".into(),
            "soul".into(),
            dummy_input_port(),
        );
        let entries = state.entries.blocking_read();
        assert!(entries.is_empty());
        assert_eq!(state.system_prompt, "system");
        assert_eq!(state.identity_prompt, "identity");
        assert_eq!(state.soul_prompt, "soul");
    }

    #[tokio::test]
    async fn test_push_entry_updates_state() {
        let state = GatewayState::new("sys".into(), "id".into(), "soul".into(), dummy_input_port());
        let entry = Entry {
            id: 0,
            parent_id: None,
            message: Message::User {
                content: UserContent::Text("hello".into()),
            },
            origin: EntryOrigin::User { channel: "test".into() },
            channel_metadata: None,
        };
        state.entries.write().await.push(entry);

        let entries = state.entries.read().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 0);
        assert_eq!(entries[0].message.content_text(), "hello");
    }
}
