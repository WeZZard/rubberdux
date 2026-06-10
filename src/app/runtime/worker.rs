//! The native app-worker entry point.
//!
//! A native app worker is a `rubberduxd` subprocess that runs a real
//! [`AgentLoop`](crate::agent::runtime::agent_loop::AgentLoop) for a single App
//! and bridges it to the host over the length-prefixed RPC protocol in
//! [`crate::protocol`]. It is the out-of-process counterpart to the in-process
//! `MemorySupervisor` worker: same `AgentLoopBuilder` spawn shape, but its
//! `OutputPort` is forwarded to the host as
//! [`AgentToHost::EntryNotification`](crate::protocol::AgentToHost::EntryNotification)
//! frames and its `InputPort` is fed from
//! [`HostToAgent::UserMessage`](crate::protocol::HostToAgent::UserMessage)
//! frames, instead of being wired to broadcast channels inside the host process.
//!
//! The first frame it sends is always
//! [`AgentToHost::Hello`](crate::protocol::AgentToHost::Hello) so the host can
//! route this socket to the matching App without relying on accept order. The
//! lifecycle and routing rationale is in
//! `docs/app/runtime/worker-lifecycle.md`. The supervisor that *spawns* this
//! process, the peer broker behind the peer frames, and tombstoning are out of
//! scope here.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::Mutex;

use crate::agent::builder::AgentLoopBuilder;
use crate::agent::entry::EntryOrigin;
use crate::error::Error;
use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::provider::moonshot::{Message, MoonshotClient, UserContent};
use crate::session::SessionManager;

/// Run a native app worker: connect to the host, announce the App with a
/// `Hello` frame, then drive a real `AgentLoop` while bridging entries out and
/// user messages in over RPC.
///
/// `app_session_dir` is the App's on-disk home directory: the worker roots its
/// [`SessionManager`] there so all session data lands under the App's directory
/// rather than the host's home.
pub async fn run_app_worker(rpc_host: String, app_id: String, app_session_dir: PathBuf) {
    if let Err(e) = run_app_worker_inner(&rpc_host, &app_id, app_session_dir).await {
        log::error!("[app-worker:{}] exited with error: {}", app_id, e);
    }
}

async fn run_app_worker_inner(
    rpc_host: &str,
    app_id: &str,
    app_session_dir: PathBuf,
) -> Result<(), Error> {
    let stream = TcpStream::connect(rpc_host)
        .await
        .map_err(|e| Error::Rpc(format!("connect to host {rpc_host}: {e}")))?;
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));

    // The Hello frame must be the first thing on the wire so the host can route
    // this socket to the right App. See docs/app/runtime/worker-lifecycle.md.
    {
        let mut w = writer.lock().await;
        protocol::write_message(
            &mut w,
            &AgentToHost::Hello {
                app_id: app_id.to_string(),
            },
        )
        .await?;
    }

    // Root a SessionManager at the App's home directory so the worker's session
    // data lives under the App, mirroring the in-process supervisor's per-App
    // session creation in crate::app::supervisor.
    let session_manager = Arc::new(session_manager_at(app_session_dir));
    let client = Arc::new(MoonshotClient::from_env());

    let (session_id, _session_dir) = session_manager
        .create_session(client.model().to_owned())
        .map_err(|e| Error::App(format!("create session for App `{app_id}`: {e}")))?;

    // The App worker runs the same builder shape as the in-process supervisor
    // worker (crate::app::supervisor::MemorySupervisor::spawn_worker): a real
    // AgentLoop whose entries are observed, here forwarded over RPC.
    let builder = AgentLoopBuilder::new(String::new(), session_manager).with_session_id(session_id);
    let (agent_loop, input_port, _context_tx) = builder.build(client).await;

    // Subscribe before run() so no entry is missed between spawn and the first
    // observation; forward every notification to the host as an RPC frame.
    let mut output = agent_loop.subscribe_output();
    let entry_writer = writer.clone();
    let entry_app_id = app_id.to_string();
    let entry_task = tokio::spawn(async move {
        while let Some(notification) = output.recv().await {
            let msg = AgentToHost::EntryNotification {
                entry: notification.entry,
                is_final: notification.is_final,
            };
            let mut w = entry_writer.lock().await;
            if let Err(e) = protocol::write_message(&mut w, &msg).await {
                log::error!(
                    "[app-worker:{}] failed to forward entry notification: {}",
                    entry_app_id,
                    e
                );
                break;
            }
        }
    });

    let loop_task = tokio::spawn(async move {
        agent_loop.run().await;
    });

    // Bridge host → worker: feed user messages into the loop's InputPort and
    // honor shutdown. Interaction and peer frames are declared by the protocol;
    // their host-side brokers are wired in later tasks.
    bridge_host_messages(&mut reader, &input_port, app_id).await;

    entry_task.abort();
    loop_task.abort();
    Ok(())
}

/// Build a [`SessionManager`] rooted at `home`, honoring the `--app-session-dir`
/// flag without mutating process-global environment. Mirrors
/// [`SessionManager::new`]'s directory layout.
fn session_manager_at(home: PathBuf) -> SessionManager {
    let sessions_dir = home.join("sessions");
    let latest_link = home.join("latest");
    SessionManager {
        home_dir: home,
        sessions_dir,
        latest_link,
    }
}

/// Read host frames until disconnect or shutdown, bridging `UserMessage` into
/// the worker's loop. Unhandled frames (peer/interaction answers) are logged;
/// their handling lands with the brokers that produce them.
async fn bridge_host_messages(
    reader: &mut OwnedReadHalf,
    input_port: &crate::agent::runtime::port::InputPort,
    app_id: &str,
) {
    loop {
        match protocol::read_message::<HostToAgent>(reader).await {
            Ok(Some(HostToAgent::UserMessage { text, .. })) => {
                let message = Message::User {
                    content: UserContent::Text(text),
                };
                if let Err(e) = input_port
                    .send_user_message(message, EntryOrigin::User { channel: "board".into() })
                    .await
                {
                    log::error!("[app-worker:{}] failed to enqueue user message: {}", app_id, e);
                    break;
                }
            }
            Ok(Some(HostToAgent::Shutdown)) => {
                log::info!("[app-worker:{}] received shutdown", app_id);
                break;
            }
            Ok(Some(other)) => {
                log::debug!("[app-worker:{}] unhandled host frame: {:?}", app_id, other);
            }
            Ok(None) => {
                log::info!("[app-worker:{}] host disconnected", app_id);
                break;
            }
            Err(e) => {
                log::error!("[app-worker:{}] error reading from host: {}", app_id, e);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_manager_at_roots_layout_under_home() {
        let home = PathBuf::from("/tmp/app-123");
        let mgr = session_manager_at(home.clone());
        assert_eq!(mgr.home_dir, home);
        assert_eq!(mgr.sessions_dir, home.join("sessions"));
        assert_eq!(mgr.latest_link, home.join("latest"));
    }
}
