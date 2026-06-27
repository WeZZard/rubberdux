use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use teloxide::prelude::Bot;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::error::Error;
use crate::protocol::{self, AgentToHost, HostToAgent};
use crate::vm::manager::VMManager;

const DEFAULT_RPC_PORT: u16 = 19384;

/// Default TCP port for the surface-client listener: the host endpoint the macOS
/// GUI client connects to so the host's [`SurfaceRouter`] can relay surface frames
/// between it and the App's worker subprocess. Distinct from the worker RPC port
/// ([`DEFAULT_RPC_PORT`] 19384, which routes worker `Hello`s) and the gateway HTTP
/// port (19385). Overridable via `RUBBERDUX_SURFACE_PORT`. The macOS client (W5)
/// dials this port and registers with an `AgentToHost::Hello { app_id }` frame.
const DEFAULT_SURFACE_PORT: u16 = 19386;

/// Bound on a surface client's outbound drive queue. Drives arrive at human
/// interaction speed; a small buffer absorbs bursts without unbounded growth.
const SURFACE_CLIENT_DRIVE_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Peer broker: the directory + relay (a switch, not an orchestrator)
// ---------------------------------------------------------------------------

/// A live delivery sink for one local App worker: the channel the broker writes a
/// resolved [`PeerDeliver`](crate::protocol::HostToAgent::PeerDeliver) frame into.
/// Held only while the worker is running; absent for a tombstoned or archived App,
/// for which the broker queues to the inbox instead. The broker treats the sink as
/// opaque — it forwards an envelope and makes no decision about its contents.
pub type PeerSink = tokio::sync::mpsc::Sender<crate::protocol::HostToAgent>;

/// The host's peer broker: a directory + relay that resolves a
/// [`PeerId`](crate::app::peer::PeerId) to a connection and forwards opaque
/// message envelopes. It is a **switch, not an orchestrator** (design decision
/// D5, `docs/app/peer/decentralized-messaging.md`): it makes no routing or
/// coordination decisions beyond "is this target's worker live right now?".
///
/// - If the target peer has a live sink, the broker delivers the envelope to it
///   directly (the directory is refreshed on every such delivery, keeping the
///   most-recently-used ordering current).
/// - Otherwise the target is offline (tombstoned or human-archived — both are
///   still addressable): the broker queues the envelope to the target's
///   `inbox.jsonl` and signals that the App should be woken, so a restore drains
///   it. Archiving is human-facing and does **not** gate addressability.
///
/// The same resolution works for a remote peer once the broker federates over the
/// general TCP transport: a non-local [`PeerId`] is forwarded to the node that
/// owns it. That path is not wired here, but the addressing model already carries
/// the node identity so adding it later changes no call site.
pub struct PeerBroker {
    store: Arc<dyn crate::app::registry::store::AppStore>,
    directory: Mutex<crate::app::peer::directory::PeerDirectory>,
    /// Live delivery sinks keyed by peer id. A present entry means the peer's
    /// worker is running and can be delivered to directly.
    sinks: Mutex<HashMap<crate::app::peer::PeerId, PeerSink>>,
}

/// What the broker decided for one `PeerSend`: either it was delivered to a live
/// worker, or it was queued to the offline target's inbox (and the target should
/// be woken so a restore drains it). Returned so the caller — the supervisor —
/// performs the side effect it owns (restoring the App) without the broker
/// reaching into supervision: the broker stays a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerRouteOutcome {
    /// The envelope was handed to the target's live delivery sink.
    Delivered,
    /// The target's worker was not running; the envelope was queued to its inbox.
    /// The supervisor should restore the target so the inbox is drained to it.
    Queued { wake: crate::app::peer::PeerId },
    /// The sender tried to address a peer it may not (itself); nothing was sent.
    Rejected,
}

impl PeerBroker {
    /// A broker backed by the given App store, with an empty directory and no
    /// live sinks. The store is consulted only for an App's on-disk directory
    /// (`app_dir`) when queueing to an inbox; routing decisions stay minimal.
    pub fn new(store: Arc<dyn crate::app::registry::store::AppStore>) -> Self {
        Self {
            store,
            directory: Mutex::new(crate::app::peer::directory::PeerDirectory::new()),
            sinks: Mutex::new(HashMap::new()),
        }
    }

    /// Register a live worker's delivery sink and admit it to the directory,
    /// called when an App's worker becomes active. Makes the App both reachable
    /// (listed by `peer_list`) and directly deliverable.
    pub async fn register(&self, id: crate::app::peer::PeerId, sink: PeerSink) {
        self.directory.lock().await.register(id.clone());
        self.sinks.lock().await.insert(id, sink);
    }

    /// Remove a worker's sink and drop it from the directory, called when the
    /// App's worker stops (tombstone/suspend/archive). The App stays addressable:
    /// a later `PeerSend` to it is queued to its inbox.
    pub async fn unregister(&self, id: &crate::app::peer::PeerId) {
        self.sinks.lock().await.remove(id);
        self.directory.lock().await.unregister(id);
    }

    /// The peers `from` may address right now, in most-recently-used order. The
    /// answer to a worker's `PeerList`. Refreshes `from`'s own recency since
    /// asking is itself activity.
    pub async fn list_for(&self, from: &crate::app::peer::PeerId) -> Vec<crate::app::peer::PeerId> {
        let mut directory = self.directory.lock().await;
        directory.touch(from);
        directory.addressable_by(from)
    }

    /// Relay one peer message from `from` to `to`. The broker's whole job: resolve
    /// `to` to a live sink and deliver, or queue to its inbox if offline. It makes
    /// no decision about the payload — it forwards the envelope verbatim. Returns
    /// the [`PeerRouteOutcome`] so the supervisor performs any wake it owns.
    pub async fn relay(
        &self,
        from: crate::app::peer::PeerId,
        to: crate::app::peer::PeerId,
        payload: serde_json::Value,
    ) -> Result<PeerRouteOutcome, Error> {
        // The single policy gate: an App may not address itself.
        {
            let directory = self.directory.lock().await;
            if !directory.may_send(&from, &to) {
                return Ok(PeerRouteOutcome::Rejected);
            }
        }

        // Sending is activity: refresh the sender's most-recently-used position.
        self.directory.lock().await.touch(&from);

        // Try a live delivery first.
        let sink = self.sinks.lock().await.get(&to).cloned();
        if let Some(sink) = sink {
            let frame = crate::protocol::HostToAgent::PeerDeliver {
                // The protocol `from` names the sending App; the node identity
                // lives in the directory/envelope, not in this user-facing field.
                from: from.app_id.to_string(),
                // Clone so the envelope survives for the inbox fall-through if the
                // sink turns out to be closed; the common case delivers and drops
                // the original below.
                payload: payload.clone(),
            };
            if sink.send(frame).await.is_ok() {
                // A delivered-to peer is active: refresh its recency too.
                self.directory.lock().await.touch(&to);
                return Ok(PeerRouteOutcome::Delivered);
            }
            // The sink was closed out from under us (the worker is stopping):
            // fall through to the inbox so the message is not lost.
            self.unregister(&to).await;
        }

        // The target is offline (tombstoned or archived) or its sink just closed:
        // queue to its inbox and ask the caller to wake it. Both states are still
        // addressable — archiving is human-facing, orthogonal to addressability.
        let envelope = crate::app::peer::mailbox::PeerEnvelope {
            from,
            payload,
        };
        // Queue into the target's *real* addressable home (live or archive), not
        // unconditionally the live directory: an archived App lives under the
        // archive, and writing an inbox to a fresh live directory would shadow its
        // manifest. `addressable_home` resolves the correct directory so the
        // restore drains exactly what was queued.
        let app_dir = self.store.addressable_home(&to.app_id);
        crate::app::peer::mailbox::Mailbox::in_dir(&app_dir).enqueue(&envelope)?;
        Ok(PeerRouteOutcome::Queued { wake: to })
    }
}

// ---------------------------------------------------------------------------
// Surface router: per-App surface frame routing (VC-P.1)
// ---------------------------------------------------------------------------

/// Delivery sink for `HostToAgent::SurfaceDrive` frames to the macOS app
/// client. The router writes one frame per drive; the macOS client stream
/// drains it. See `docs/agent/world/ecs-runtime.md` (Theme 2a).
pub type SurfaceDriveSink = tokio::sync::mpsc::Sender<crate::protocol::HostToAgent>;

/// Inbound sink for `AgentToHost` surface frames from the macOS app client.
/// The router writes `SurfaceObservation`/`SurfaceMutated` frames here; the
/// App's ECS World or worker bridge drains them. See
/// `docs/agent/world/ecs-runtime.md` (Theme 2b; Inv 18).
pub type SurfaceInboundSink = tokio::sync::mpsc::Sender<crate::protocol::AgentToHost>;

/// Per-App surface routing table. Routes `HostToAgent::SurfaceDrive` from the
/// worker socket to the correct per-App macOS client stream, and routes inbound
/// `AgentToHost::{SurfaceObservation,SurfaceMutated}` from the client to the
/// correct App's worker seam — both keyed by App identity rather than
/// connection-accept order. Two Apps connecting in either order route correctly.
/// See `docs/agent/world/ecs-runtime.md` (Theme 2a/2b; VC-P.1).
pub struct SurfaceRouter {
    /// Per-App macOS client drive sinks. The host writes `SurfaceDrive` frames
    /// here; the macOS client stream delivers them to the native UI.
    drives: Mutex<HashMap<String, SurfaceDriveSink>>,
    /// Per-App worker inbound sinks. The macOS client writes
    /// `SurfaceObservation`/`SurfaceMutated` frames here; the worker bridge or
    /// ECS World folds them into `LogicalInput`s.
    inbound: Mutex<HashMap<String, SurfaceInboundSink>>,
}

impl SurfaceRouter {
    /// An empty router with no App registrations.
    pub fn new() -> Self {
        Self {
            drives: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
        }
    }

    /// Register the macOS app client's drive sink for `app_id`. Called when a
    /// client stream connects and identifies its App. Order-independent: two
    /// Apps may register in any order and route correctly.
    pub async fn register_client(&self, app_id: String, sink: SurfaceDriveSink) {
        self.drives.lock().await.insert(app_id, sink);
    }

    /// Register the App's worker inbound seam for `app_id`. Called when the
    /// worker's surface-observation channel is provisioned.
    pub async fn register_worker(&self, app_id: String, sink: SurfaceInboundSink) {
        self.inbound.lock().await.insert(app_id, sink);
    }

    /// Deregister the macOS client's drive sink for `app_id` (client disconnected).
    pub async fn unregister_client(&self, app_id: &str) {
        self.drives.lock().await.remove(app_id);
    }

    /// Deregister the worker's inbound seam for `app_id` (worker stopped).
    pub async fn unregister_worker(&self, app_id: &str) {
        self.inbound.lock().await.remove(app_id);
    }

    /// Route a `HostToAgent::SurfaceDrive` to `app_id`'s registered macOS
    /// client. Returns `true` if delivered. A missing or closed sink is logged
    /// and returns `false`; never blocks or propagates an error.
    pub async fn route_drive(
        &self,
        app_id: &str,
        frame: crate::protocol::HostToAgent,
    ) -> bool {
        let sink = self.drives.lock().await.get(app_id).cloned();
        if let Some(sink) = sink {
            match sink.try_send(frame) {
                Ok(()) => return true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    log::warn!(
                        "[surface-router] SurfaceDrive for App `{app_id}`: client sink full, frame dropped"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    log::warn!(
                        "[surface-router] SurfaceDrive for App `{app_id}`: client disconnected"
                    );
                    // Sink closed: client disconnected; remove the stale entry.
                    self.drives.lock().await.remove(app_id);
                }
            }
        } else {
            log::debug!(
                "[surface-router] SurfaceDrive for App `{app_id}`: no client registered"
            );
        }
        false
    }

    /// Route an inbound `AgentToHost` surface frame (`SurfaceObservation` or
    /// `SurfaceMutated`) from `app_id`'s macOS client to the App's worker seam.
    /// Returns `true` if delivered.
    pub async fn route_inbound(
        &self,
        app_id: &str,
        frame: crate::protocol::AgentToHost,
    ) -> bool {
        let sink = self.inbound.lock().await.get(app_id).cloned();
        if let Some(sink) = sink {
            match sink.try_send(frame) {
                Ok(()) => return true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    log::warn!(
                        "[surface-router] surface inbound for App `{app_id}`: worker seam full, frame dropped"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    log::warn!(
                        "[surface-router] surface inbound for App `{app_id}`: worker seam closed"
                    );
                    // Sink closed: worker stopped; remove the stale entry.
                    self.inbound.lock().await.remove(app_id);
                }
            }
        } else {
            log::debug!(
                "[surface-router] surface inbound for App `{app_id}`: no worker seam registered"
            );
        }
        false
    }
}

/// Run the surface-client accept loop: the macOS GUI client connects here so the
/// host's [`SurfaceRouter`] can relay surface frames between it and the App's
/// worker subprocess. Each accepted connection is handled on its own task; an
/// accept error is logged and the loop continues. Runs for the host's lifetime.
///
/// This is a dedicated raw-TCP length-prefixed-frame listener (the same framing
/// as the worker RPC link), following the `accept_worker` pattern. It is NOT the
/// gateway HTTP port (19385) and NOT the worker RPC port (19384, which routes
/// worker `Hello`s). See `docs/agent/world/ecs-runtime.md`.
pub async fn run_surface_client_listener(listener: TcpListener, router: Arc<SurfaceRouter>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_surface_client(stream, router).await {
                        log::warn!("[surface-client] connection from {addr} ended: {e}");
                    }
                });
            }
            Err(e) => log::error!("[surface-client] accept error: {e}"),
        }
    }
}

/// Handle one macOS surface client: perform the registration handshake, register
/// its drive sink with the router, then pump the client's inbound surface frames
/// to the App's worker.
///
/// REGISTRATION HANDSHAKE (the contract W5's macOS client targets): the client's
/// FIRST frame MUST be [`AgentToHost::Hello`] carrying the App id it observes.
/// The host then:
/// - registers a drive sink with [`SurfaceRouter::register_client`] — a spawned
///   task drains [`HostToAgent::SurfaceDrive`] frames the router routes and writes
///   each to the client socket (the host→client drive leg), and
/// - reads the client's [`AgentToHost::SurfaceObservation`]/[`SurfaceMutated`]
///   frames and routes each to the App's worker via
///   [`SurfaceRouter::route_inbound`] (the client→worker observation leg).
///
/// A non-`Hello` first frame is rejected; a clean EOF before registering is a
/// no-op. On disconnect the client's drive sink is unregistered.
async fn handle_surface_client(stream: TcpStream, router: Arc<SurfaceRouter>) -> Result<(), Error> {
    let (mut reader, mut writer) = stream.into_split();

    // The registration frame must be first on the wire so the host can route this
    // client to the right App without relying on accept order.
    let app_id = match protocol::read_message::<AgentToHost>(&mut reader).await? {
        Some(AgentToHost::Hello { app_id }) => app_id,
        Some(other) => {
            return Err(Error::Rpc(format!(
                "surface client's first frame was not a Hello registration: {other:?}"
            )));
        }
        None => return Ok(()),
    };
    log::info!("[surface-client] registered for App `{app_id}`");

    // Register the client's drive sink: a task drains the routed drive frames and
    // writes each to the client socket. The router holds only the sender, keeping
    // the relay stateless-per-frame.
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel::<HostToAgent>(SURFACE_CLIENT_DRIVE_CAPACITY);
    router.register_client(app_id.clone(), drive_tx).await;

    let drive_app_id = app_id.clone();
    let drive_task = tokio::spawn(async move {
        while let Some(frame) = drive_rx.recv().await {
            if let Err(e) = protocol::write_message(&mut writer, &frame).await {
                log::warn!("[surface-client:{drive_app_id}] failed to write drive: {e}");
                break;
            }
        }
    });

    // Inbound pump: route the client's observed/mutated surface frames to the
    // App's worker. Other frames are ignored — only surface observations cross
    // this seam.
    let result = async {
        loop {
            match protocol::read_message::<AgentToHost>(&mut reader).await? {
                Some(frame @ AgentToHost::SurfaceObservation { .. })
                | Some(frame @ AgentToHost::SurfaceMutated { .. }) => {
                    router.route_inbound(&app_id, frame).await;
                }
                Some(other) => log::debug!(
                    "[surface-client:{app_id}] ignoring non-surface frame: {other:?}"
                ),
                None => {
                    log::info!("[surface-client:{app_id}] disconnected");
                    break;
                }
            }
        }
        Ok::<(), Error>(())
    }
    .await;

    router.unregister_client(&app_id).await;
    drive_task.abort();
    result
}

/// The read/write halves of an accepted worker socket, paired so a caller can
/// keep streaming after the routing decision has been made.
pub struct WorkerStream {
    pub reader: tokio::net::tcp::OwnedReadHalf,
    pub writer: tokio::net::tcp::OwnedWriteHalf,
}

/// The outcome of inspecting an accepted worker socket's first frame.
///
/// Incoming worker sockets are routed by their first frame rather than by
/// accept order: a native app worker opens with
/// [`AgentToHost::Hello`](crate::protocol::AgentToHost::Hello), while a VM child
/// opens with a task frame ([`AgentToHost::Response`] /
/// [`AgentToHost::ExternalInteraction`]). See
/// `docs/app/runtime/worker-lifecycle.md`.
pub enum AcceptedWorker {
    /// A native app worker identified by its `Hello` frame.
    App {
        app_id: String,
        stream: WorkerStream,
    },
    /// A VM child connection. `first_frame` is the frame already read off the
    /// wire while classifying the socket, handed back so the VM path processes
    /// it exactly as before.
    VmChild {
        first_frame: AgentToHost,
        stream: WorkerStream,
    },
}

/// Accept one worker connection and classify it by its first frame.
///
/// This replaces accept-by-order routing: the first frame decides whether the
/// socket belongs to a native app worker (`Hello`) or a VM child (a task frame).
/// The VM child's first frame is returned so no message is lost.
pub async fn accept_worker(listener: &TcpListener) -> Result<AcceptedWorker, Error> {
    let (stream, addr) = listener.accept().await?;
    let (mut reader, writer) = stream.into_split();
    match protocol::read_message::<AgentToHost>(&mut reader).await? {
        Some(AgentToHost::Hello { app_id }) => {
            log::info!("App worker {} connected from {}", app_id, addr);
            Ok(AcceptedWorker::App {
                app_id,
                stream: WorkerStream { reader, writer },
            })
        }
        Some(first_frame) => {
            log::info!("VM child connected from {}", addr);
            Ok(AcceptedWorker::VmChild {
                first_frame,
                stream: WorkerStream { reader, writer },
            })
        }
        None => Err(Error::Rpc(format!(
            "worker at {addr} disconnected before sending any frame"
        ))),
    }
}

/// Configuration for host mode.
#[derive(Clone)]
pub struct HostConfig {
    pub vm_image: String,
    pub share_root: PathBuf,
    pub rpc_port: u16,
    /// TCP port the macOS GUI surface client connects to. See
    /// [`DEFAULT_SURFACE_PORT`] and `docs/agent/world/ecs-runtime.md`.
    pub surface_port: u16,
    pub host_ip: String,
    pub agent_binary_path: Option<String>,
    pub agent_env: HashMap<String, String>,
    pub agent_data_dir: Option<PathBuf>,
    pub memory_mb: Option<usize>,
    pub cpu_count: Option<usize>,
}

impl HostConfig {
    pub fn from_env() -> Self {
        let image = std::env::var("RUBBERDUX_VM_IMAGE")
            .ok()
            .map(|raw| {
                crate::vm::setup::get_image(&raw)
                    .map(|img| img.base_vm_name.to_string())
                    .unwrap_or(raw)
            })
            .unwrap_or_else(|| "rubberdux-base-ubuntu24-release".to_string());

        let share_root = std::env::var("RUBBERDUX_VM_SHARES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./vm-shares"));

        let rpc_port: u16 = std::env::var("RUBBERDUX_RPC_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RPC_PORT);

        let surface_port: u16 = std::env::var("RUBBERDUX_SURFACE_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_SURFACE_PORT);

        let host_ip =
            std::env::var("RUBBERDUX_HOST_IP").unwrap_or_else(|_| "192.168.64.1".to_string());

        let agent_data_dir = std::env::var("RUBBERDUX_AGENT_DATA_DIR")
            .map(PathBuf::from)
            .ok();

        // Propagate LLM configuration to the agent VM
        let mut agent_env = HashMap::new();
        for key in [
            "RUBBERDUX_LLM_BASE_URL",
            "RUBBERDUX_LLM_API_KEY",
            "RUBBERDUX_LLM_MODEL",
            "RUBBERDUX_LLM_USER_AGENT",
        ] {
            if let Ok(value) = std::env::var(key) {
                agent_env.insert(key.to_string(), value);
            }
        }

        Self {
            vm_image: image,
            share_root,
            rpc_port,
            surface_port,
            host_ip,
            agent_binary_path: None,
            agent_env,
            agent_data_dir,
            memory_mb: None,
            cpu_count: None,
        }
    }
}

fn build_agent_command(config: &HostConfig, task_id: Option<&str>) -> String {
    let binary = config.agent_binary_path.as_deref().unwrap_or("rubberduxd");
    let binary_quoted = shell_quote(binary);
    let mut cmd = format!(
        "{} --agent --rpc-host {}:{}",
        binary_quoted, config.host_ip, config.rpc_port
    );
    if let Some(tid) = task_id {
        cmd.push_str(&format!(" --task-id {}", shell_quote(tid)));
    }

    // Ensure the binary is executable and strip quarantine attributes (macOS)
    let mut setup = if config.agent_binary_path.is_some() {
        format!(
            "chmod +x {} && xattr -d com.apple.quarantine {} 2>/dev/null || true && ",
            binary_quoted, binary_quoted
        )
    } else {
        String::new()
    };

    // Set up persistent data directory symlinks inside the VM
    if config.agent_data_dir.is_some() {
        setup.push_str(
            "OS=\"$(uname -s)\"; \
            if [[ \"$OS\" == \"Darwin\" ]]; then \
                mkdir -p \"/Volumes/My Shared Files/data/\"{documents,downloads,config,sessions,tool-results,subagents}; \
                ln -sf \"/Volumes/My Shared Files/data/documents\" ~/Documents; \
                ln -sf \"/Volumes/My Shared Files/data/downloads\" ~/Downloads; \
                ln -sf \"/Volumes/My Shared Files/data/config\" ~/.rubberdux; \
                export RUBBERDUX_DATA_DIR=\"/Volumes/My Shared Files/data\"; \
            elif [[ \"$OS\" == \"Linux\" ]]; then \
                sudo mkdir -p /mnt/shared; \
                sudo mount -t virtiofs com.apple.virtio-fs.automount /mnt/shared 2>/dev/null || true; \
                mkdir -p /mnt/shared/data/{documents,downloads,config,sessions,tool-results,subagents}; \
                ln -sf /mnt/shared/data/documents ~/Documents; \
                ln -sf /mnt/shared/data/downloads ~/Downloads; \
                ln -sf /mnt/shared/data/config ~/.rubberdux; \
                export RUBBERDUX_DATA_DIR=\"/mnt/shared/data\"; \
            fi && "
        );
    }

    let cmd = setup + &cmd;

    if config.agent_env.is_empty() {
        format!("nohup {} > /tmp/rubberdux-agent.log 2>&1 &", cmd)
    } else {
        let exports: Vec<String> = config
            .agent_env
            .iter()
            .map(|(k, v)| format!("export {}={}", shell_quote(k), shell_quote(v)))
            .collect();
        let script = exports.join(" && ") + " && " + &cmd;
        format!(
            "nohup bash -c {} > /tmp/rubberdux-agent.log 2>&1 &",
            shell_quote(&script)
        )
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Run rubberdux in host mode.
///
/// The host runs the AgentLoop locally and bridges Telegram ↔ AgentLoop
/// via the broadcast-based adapter.
pub async fn run(config: HostConfig, bot: Option<Bot>) {
    use crate::agent::builder::AgentLoopBuilder;

    let rpc_listener = match TcpListener::bind(("0.0.0.0", config.rpc_port)).await {
        Ok(l) => {
            log::info!("RPC listener bound to 0.0.0.0:{}", config.rpc_port);
            Arc::new(l)
        }
        Err(e) => {
            log::warn!("Failed to bind RPC listener on port {}: {} (VM isolation disabled)", config.rpc_port, e);
            // Continue without VM support — isolate=true will return an error.
            // If even the loopback fallback cannot bind, the host has no RPC
            // transport at all: log and abort startup rather than panicking.
            match TcpListener::bind("127.0.0.1:0").await {
                Ok(l) => Arc::new(l),
                Err(e) => {
                    log::error!("Failed to bind fallback RPC listener: {e}; aborting host startup");
                    return;
                }
            }
        }
    };

    // The surface router relays surface frames between each App's worker
    // subprocess and the registered macOS GUI client, keyed by App id. The same
    // router is shared with the surface-client listener (which registers clients
    // and routes their observations inbound) and the App supervisor (whose per-App
    // pump registers the worker and routes its drives outbound). See
    // `docs/agent/world/ecs-runtime.md`.
    let surface_router = Arc::new(SurfaceRouter::new());

    // Stand up the surface-client listener: the dedicated host endpoint the macOS
    // GUI client (W5) connects to. A bind failure disables the live surface relay
    // but leaves the rest of the host serving.
    match TcpListener::bind(("0.0.0.0", config.surface_port)).await {
        Ok(listener) => {
            log::info!("Surface-client listener bound to 0.0.0.0:{}", config.surface_port);
            tokio::spawn(run_surface_client_listener(listener, surface_router.clone()));
        }
        Err(e) => log::warn!(
            "Failed to bind surface-client listener on port {}: {e} (live surface relay disabled)",
            config.surface_port
        ),
    }

    let mut vm_manager = VMManager::new(config.vm_image.clone(), config.share_root.clone());
    if let Some(mem) = config.memory_mb {
        vm_manager = vm_manager.with_memory_mb(mem);
    }
    if let Some(cpus) = config.cpu_count {
        vm_manager = vm_manager.with_cpu_count(cpus);
    }
    let vm_manager = Arc::new(Mutex::new(vm_manager));
    let host_config = Arc::new(config);

    // Initialize session manager and create new session
    let session_manager = Arc::new(crate::session::SessionManager::new());
    let model = std::env::var("RUBBERDUX_LLM_MODEL").unwrap_or_else(|_| "kimi-for-coding".into());
    let (session_id, session_dir) = match session_manager.create_session(model) {
        Ok(session) => session,
        Err(e) => {
            log::error!("Failed to create session: {e}; aborting host startup");
            return;
        }
    };

    log::info!(
        "Created session: {} at {}",
        session_id.to_string(),
        session_dir.display()
    );

    // Create project root symlink if missing
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !project_root.join("sessions").exists() {
        if let Err(e) = crate::session::SessionManager::create_project_symlink(&project_root) {
            log::warn!("Failed to create project sessions symlink: {}", e);
        }
    }

    let mindset = Arc::new(crate::mindset::Mindset::new());
    if let Err(e) = mindset.ensure_dirs() {
        log::error!("Failed to initialize mindset: {e}; aborting host startup");
        return;
    }
    mindset.seed_defaults_if_empty(&project_root.join("prompts"));
    log::info!("Mindset root: {}", mindset.root.display());

    let workspace = Arc::new(crate::workspace::Workspace::new());
    if let Err(e) = workspace.ensure_dirs() {
        log::error!("Failed to initialize workspace: {e}; aborting host startup");
        return;
    }

    log::info!("Workspace root: {}", workspace.root.display());

    let interaction_queue = std::sync::Arc::new(
        crate::agent::external::interaction_queue::InteractionQueue::new(),
    );

    let mut conventions = crate::guardrail::ConventionRegistry::new();
    conventions.register(crate::channel::adapter::telegram_guardrails::convention());
    conventions.register(crate::workspace::convention());
    conventions.register(crate::mindset::convention());
    conventions.register(crate::agent::external::convention(interaction_queue.clone()));

    let convention_guidance = conventions.compose_guidance();
    let guardrails = conventions.build_guardrail_chain();

    let mut prompt_parts = crate::hardened_prompts::load_prompt_parts(&mindset.root);
    prompt_parts.push(convention_guidance);
    let channel_partial = Some(crate::channel::adapter::telegram::channel_prompt());
    let system_prompt =
        crate::hardened_prompts::compose_system_prompt(&prompt_parts, channel_partial);

    let client = Arc::new(crate::provider::moonshot::MoonshotClient::from_env());

    // Create the trajectory broadcast channel and recorder before building the
    // agent loop so that events flow to both the filesystem log and any
    // WebSocket subscribers on `/api/v1/ws/trajectory`.
    let (trajectory_tx, _) = tokio::sync::broadcast::channel(256);
    let events_path = session_manager
        .main_agent_dir(&session_id)
        .join("events.jsonl");
    let gateway_events_path = events_path.clone();
    let fs_recorder = crate::trajectory::filesystem_recorder(events_path);
    let broadcast_recorder: crate::trajectory::SharedTrajectoryRecorder = Arc::new(
        crate::trajectory::BroadcastTrajectoryRecorder::new(
            fs_recorder,
            trajectory_tx.clone(),
        ),
    );

    let gateway_system_prompt = system_prompt.clone();
    let telegram_chat_id: std::sync::Arc<tokio::sync::Mutex<Option<i64>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let mut builder = AgentLoopBuilder::new(system_prompt, session_manager)
        .with_session_id(session_id)
        .with_workspace(workspace)
        .with_mindset(mindset.clone())
        .with_guardrails(guardrails)
        .with_recorder(broadcast_recorder)
        .with_external_cwd(project_root.clone())
        .with_interaction_queue(interaction_queue.clone())
        .with_vm_infrastructure(vm_manager, rpc_listener, host_config);
    // Register the Telegram channel processor only when a bot is configured; the
    // gateway and app-board channels function without it.
    if let Some(bot) = bot.as_ref() {
        let telegram_processor = std::sync::Arc::new(
            crate::channel::adapter::telegram::TelegramChannelProcessor::new(
                bot.clone(),
                telegram_chat_id.clone(),
            )
            .with_interaction_queue(interaction_queue.clone()),
        );
        builder = builder.with_channel_processor("telegram", telegram_processor);
    }
    let (agent_loop, input_port, _context_tx) = builder.build(client).await;

    // Subscribe to entry broadcasts for the Telegram adapter
    let entry_rx = agent_loop.subscribe_output().into_receiver();

    // Bring up the multi-App board subsystem additively: the default
    // subprocess-backed `AppSupervisor` (`LocalSupervisor`) owns the
    // `Hello`-routing accept loop, the peer broker, and the idle sweeper, all set
    // up by `bind`. Apps load `Tombstoned` from the filesystem store at startup;
    // no worker spawns until an App is used. The supervisor is `None` when binding
    // fails, in which case the board surface is simply absent while the rest of
    // the host comes up unaffected. See `docs/app/runtime/worker-lifecycle.md`.
    let app_store: Arc<dyn crate::app::registry::store::AppStore> =
        Arc::new(crate::app::registry::store::FilesystemAppStore::new());
    // `bind_shared` returns the supervisor already in an `Arc` so each App's
    // supervision pump holds a live `Weak<Self>` and can wake an offline target
    // through `ensure_active` when a `PeerSend` is queued to its inbox. See
    // `docs/app/peer/decentralized-messaging.md`.
    let app_supervisor =
        match crate::app::runtime::local_supervisor::LocalSupervisor::bind_shared(
            app_store.clone(),
            surface_router.clone(),
        )
        .await
        {
            Ok(supervisor) => {
                match app_store.list(false) {
                    Ok(apps) => log::info!(
                        "App board ready: {} App(s) loaded Tombstoned at startup",
                        apps.len()
                    ),
                    Err(e) => log::warn!("Failed to enumerate Apps at startup: {e}"),
                }
                Some(supervisor)
            }
            Err(e) => {
                log::warn!("Failed to bind App supervisor: {e}; board surface disabled");
                None
            }
        };

    // Set up the gateway server
    let _gateway_handle = {
        let output_port = agent_loop.subscribe_output();
        let identity = std::fs::read_to_string(mindset.identity_path()).unwrap_or_default();
        let soul = std::fs::read_to_string(mindset.soul_path()).unwrap_or_default();
        let mut gateway_state = crate::gateway::state::GatewayState::with_trajectory_tx(
            gateway_system_prompt, identity, soul, trajectory_tx, input_port.clone(),
            Some(gateway_events_path),
        );
        // Light up the board REST + WS routes against the supervisor while keeping
        // the single-agent surface above. The identity client derives an App's
        // title + icon as a background task on creation. See `docs/gateway/apps.md`.
        if let Some(supervisor) = app_supervisor {
            let identity_client = Arc::new(crate::provider::moonshot::MoonshotClient::from_env());
            gateway_state.attach_apps(supervisor, identity_client);
        }
        let gateway_state = Arc::new(gateway_state);
        let state_clone = gateway_state.clone();
        tokio::spawn(crate::gateway::stream::mirror_entries(output_port, state_clone));
        tokio::spawn(crate::gateway::server::run(gateway_state))
    };

    // Spawn AgentLoop
    tokio::spawn(async move {
        agent_loop.run().await;
    });

    match bot {
        // Run the Telegram adapter (blocks until the dispatcher shuts down).
        Some(bot) => {
            crate::channel::adapter::telegram::run(
                bot,
                input_port,
                entry_rx,
                telegram_chat_id,
                interaction_queue,
            )
            .await;
        }
        // No Telegram bridge: keep the host alive so the gateway, app board, and
        // agent loop keep serving until the process is interrupted.
        None => {
            log::info!("Host ready (Telegram bridge disabled). Press Ctrl-C to stop.");
            if let Err(e) = tokio::signal::ctrl_c().await {
                log::error!("Failed to listen for shutdown signal: {e}");
            }
        }
    }

    log::info!("Host shutdown complete.");
}

/// Run a child VM to completion and return the final output.
/// Guarantees the child VM is destroyed even if the agent fails or errors occur.
pub async fn run_child_vm(
    manager: Arc<Mutex<VMManager>>,
    task_id: &str,
    prompt: &str,
    subagent_type: &str,
    agent_name: Option<&str>,
    config: &HostConfig,
    listener: Arc<TcpListener>,
    interaction_queue: std::sync::Arc<crate::agent::external::interaction_queue::InteractionQueue>,
) -> Result<String, Error> {
    // Helper to write status updates to the child share for debugging
    async fn write_status(share_dir: &std::path::Path, msg: &str) {
        let _ = tokio::fs::write(share_dir.join("status.txt"), msg).await;
    }

    // Create and start child VM
    {
        let mut mgr = manager.lock().await;
        write_status(&mgr.share_dir(task_id), "run_child_vm: creating VM").await;
        mgr.create_and_start(task_id, None, config.agent_data_dir.as_deref())
            .await?;
    }

    // Run the child VM lifecycle with guaranteed cleanup
    let result = async {
        // Wait for SSH
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: waiting for SSH").await;
            mgr.wait_for_ssh(task_id).await?;
        }

        // Copy the agent binary from the main VM share to the child VM share
        // so the child can execute it.
        {
            let mgr = manager.lock().await;
            write_status(
                &mgr.share_dir(task_id),
                "run_child_vm: copying binary and prompt",
            )
            .await;
            let main_binary = config.share_root.join("main").join("rubberduxd");
            let child_binary = mgr.share_dir(task_id).join("rubberduxd");
            if main_binary.exists() {
                tokio::fs::copy(&main_binary, &child_binary).await?;
            }
            let prompt_path = mgr.share_dir(task_id).join("prompt.txt");
            tokio::fs::write(&prompt_path, prompt).await?;
            let subagent_type_path = mgr.share_dir(task_id).join("subagent_type.txt");
            tokio::fs::write(&subagent_type_path, subagent_type).await?;
            if let Some(name) = agent_name {
                let agent_name_path = mgr.share_dir(task_id).join("agent_name.txt");
                tokio::fs::write(&agent_name_path, name).await.map_err(|e| {
                    Error::Vm(format!("Failed to write agent_name.txt: {}", e))
                })?;
                log::info!("Wrote agent_name.txt: {}", name);
            }
        }

        // Start the agent inside the child VM
        let agent_cmd = build_agent_command(config, Some(task_id));
        {
            let mgr = manager.lock().await;
            write_status(&mgr.share_dir(task_id), "run_child_vm: starting agent").await;
            let result = mgr.exec(task_id, &agent_cmd).await?;
            if result.exit_code != 0 {
                let err = format!(
                    "Child VM agent failed to start (exit {}): stdout={} stderr={}",
                    result.exit_code, result.stdout, result.stderr
                );
                write_status(&mgr.share_dir(task_id), &err).await;
                return Err(Error::Vm(err));
            }
        }

        // Copy child VM agent log to share immediately so it survives even if
        // listener.accept() hangs (helps debugging connection issues).
        {
            let mgr = manager.lock().await;
            let log_result = mgr
                .exec(task_id, "cat /tmp/rubberdux-agent.log 2>/dev/null || true")
                .await;
            let early_log = log_result.map(|r| r.stdout).unwrap_or_default();
            let log_path = mgr.share_dir(task_id).join("agent.log");
            let _ = tokio::fs::write(&log_path, &early_log).await;
            write_status(
                &mgr.share_dir(task_id),
                "run_child_vm: waiting for RPC connection",
            )
            .await;
        }

        // Accept the child's RPC connection, routing by the first frame instead
        // of by accept order. A native app worker would open with `Hello`; this
        // VM path only consumes VM-child sockets and hands any stray app socket
        // back to the listener for the supervisor (wired in a later task) by
        // closing it — no app worker is expected on this path.
        // See docs/app/runtime/worker-lifecycle.md.
        let (mut reader, writer, first_frame) = loop {
            match accept_worker(&listener).await? {
                AcceptedWorker::VmChild { first_frame, stream } => {
                    log::info!("Child VM {} matched a VM-child socket", task_id);
                    break (stream.reader, stream.writer, first_frame);
                }
                AcceptedWorker::App { app_id, stream } => {
                    log::warn!(
                        "App worker {} connected on VM-child accept path; closing (no supervisor here)",
                        app_id
                    );
                    drop(stream);
                }
            }
        };
        let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

        // Process the first frame that was already read while classifying the
        // socket, then continue reading until the child's final response.
        let mut final_text = String::new();
        let mut pending = Some(first_frame);
        loop {
            let msg: Option<AgentToHost> = match pending.take() {
                Some(frame) => Some(frame),
                None => protocol::read_message(&mut reader).await?,
            };
            match msg {
                Some(AgentToHost::Response { text, is_final, .. }) => {
                    final_text = text;
                    if is_final {
                        break;
                    }
                }
                Some(AgentToHost::SpawnVM { .. }) => {
                    // Defensive guard: child VMs no longer have the agent tool,
                    // so this should never happen. Log and ignore.
                    log::warn!("Child VM {} requested nested spawn (ignoring)", task_id);
                }
                Some(AgentToHost::ExternalInteraction { task_id: _ext_task_id, request }) => {
                    let request_id = crate::agent::external::get_request_id(&request).to_string();
                    log::info!("VM child {} sent ExternalInteraction: {}", task_id, request_id);

                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    interaction_queue.add(
                        request_id.clone(),
                        crate::agent::external::interaction_queue::PendingInteraction {
                            request,
                            response_tx: resp_tx,
                        },
                    );

                    // Spawn task to write response back to VM when interaction is resolved
                    let writer_clone = writer.clone();
                    tokio::spawn(async move {
                        if let Ok(response) = resp_rx.await {
                            let msg = crate::protocol::HostToAgent::InteractionResponse {
                                request_id,
                                response,
                            };
                            let mut w = writer_clone.lock().await;
                            if let Err(e) = crate::protocol::write_message(&mut *w, &msg).await {
                                log::warn!("Failed to send InteractionResponse to VM: {}", e);
                            }
                        }
                    });
                }
                Some(other) => {
                    // App-worker frames (Hello/EntryNotification/peer/interaction)
                    // never arrive on the VM-child path — sockets are classified
                    // by their first frame in `accept_worker`. Log defensively.
                    log::warn!(
                        "Child VM {} sent unexpected frame on VM path: {:?}",
                        task_id,
                        other
                    );
                }
                None => {
                    log::info!("Child VM {} disconnected", task_id);
                    break;
                }
            }
        }

        // Copy child VM agent log to share for debugging before destruction
        {
            let mgr = manager.lock().await;
            let log_result = mgr
                .exec(task_id, "cat /tmp/rubberdux-agent.log 2>/dev/null || true")
                .await;
            let log_content = log_result.map(|r| r.stdout).unwrap_or_default();
            let log_path = mgr.share_dir(task_id).join("agent.log");
            let _ = tokio::fs::write(&log_path, &log_content).await;
            // Also persist on the host filesystem so it survives share cleanup
            let host_log_path =
                std::path::PathBuf::from(format!("/tmp/rubberdux-child-{}.log", task_id));
            let _ = tokio::fs::write(&host_log_path, &log_content).await;
        }

        Ok(final_text)
    }
    .await;

    // Destroy the child VM regardless of success or failure
    {
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.destroy(task_id).await {
            log::warn!("Failed to destroy child VM {}: {}", task_id, e);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct EnvVarGuard {
        key: &'static str,
        value: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, value }
        }

        fn set(key: &'static str, new_value: &str) -> Self {
            let value = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, new_value);
            }
            Self { key, value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(value) = &self.value {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn test_shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[test]
    #[serial(host_config_env)]
    fn test_host_config_from_env_defaults() {
        let _guard = EnvVarGuard::unset("RUBBERDUX_RPC_PORT");
        // Test that HostConfig::from_env() doesn't panic when env vars are missing
        // by setting required vars if not present
        let config = HostConfig::from_env();
        assert_eq!(config.rpc_port, DEFAULT_RPC_PORT);
    }

    #[test]
    #[serial(host_config_env)]
    fn test_host_config_custom_rpc_port() {
        let _guard = EnvVarGuard::set("RUBBERDUX_RPC_PORT", "12345");
        let config = HostConfig::from_env();
        assert_eq!(config.rpc_port, 12345);
    }

    #[test]
    fn test_build_agent_command_basic() {
        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            surface_port: DEFAULT_SURFACE_PORT,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: HashMap::new(),
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, None);
        assert!(cmd.contains("rubberduxd"));
        assert!(cmd.contains("--agent"));
        assert!(cmd.contains("192.168.64.1:19384"));
    }

    #[test]
    fn test_build_agent_command_with_task_id() {
        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            surface_port: DEFAULT_SURFACE_PORT,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: HashMap::new(),
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, Some("task-123"));
        assert!(cmd.contains("--task-id"));
        assert!(cmd.contains("task-123"));
    }

    // -- PeerBroker: directory + relay (a switch, not an orchestrator) --------

    fn broker_store() -> Arc<dyn crate::app::registry::store::AppStore> {
        let dir = tempfile::tempdir().unwrap().into_path();
        Arc::new(crate::app::registry::store::FilesystemAppStore::with_apps_dir(dir))
    }

    fn peer(app: &str) -> crate::app::peer::PeerId {
        crate::app::peer::PeerId::local(crate::app::AppId(app.into()))
    }

    #[tokio::test]
    async fn relay_delivers_to_a_live_sink() {
        let broker = PeerBroker::new(broker_store());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        broker.register(peer("b"), tx).await;

        let outcome = broker
            .relay(peer("a"), peer("b"), serde_json::json!({"text": "ping"}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Delivered);

        match rx.recv().await.unwrap() {
            crate::protocol::HostToAgent::PeerDeliver { from, payload } => {
                assert_eq!(from, "a");
                assert_eq!(payload["text"], "ping");
            }
            other => panic!("expected PeerDeliver, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn relay_queues_to_inbox_when_target_offline() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        // No sink registered for `b`: it is offline (tombstoned or archived).
        let outcome = broker
            .relay(peer("a"), peer("b"), serde_json::json!({"text": "later"}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Queued { wake: peer("b") });

        // The envelope landed in b's inbox, ready to drain on restore.
        let app_dir = store.app_dir(&crate::app::AppId("b".into()));
        let queued = crate::app::peer::mailbox::Mailbox::in_dir(&app_dir)
            .drain()
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].from, peer("a"));
        assert_eq!(queued[0].payload["text"], "later");
    }

    #[tokio::test]
    async fn relay_rejects_self_addressing() {
        let broker = PeerBroker::new(broker_store());
        let outcome = broker
            .relay(peer("a"), peer("a"), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Rejected);
    }

    #[tokio::test]
    async fn list_for_excludes_self_and_is_mru_ordered() {
        let broker = PeerBroker::new(broker_store());
        let (tx_b, _rx_b) = tokio::sync::mpsc::channel(1);
        let (tx_c, _rx_c) = tokio::sync::mpsc::channel(1);
        broker.register(peer("a"), tokio::sync::mpsc::channel(1).0).await;
        broker.register(peer("b"), tx_b).await;
        broker.register(peer("c"), tx_c).await;

        let listed = broker.list_for(&peer("a")).await;
        assert!(!listed.contains(&peer("a")), "must not list self");
        // c registered most recently among the others.
        assert_eq!(listed, vec![peer("c"), peer("b")]);
    }

    #[tokio::test]
    async fn unregister_keeps_app_addressable_via_inbox() {
        let store = broker_store();
        let broker = PeerBroker::new(store.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        broker.register(peer("b"), tx).await;
        broker.unregister(&peer("b")).await;

        // Gone from the directory, but still addressable: the message is queued.
        assert!(!broker.list_for(&peer("a")).await.contains(&peer("b")));
        let outcome = broker
            .relay(peer("a"), peer("b"), serde_json::json!({"text": "x"}))
            .await
            .unwrap();
        assert_eq!(outcome, PeerRouteOutcome::Queued { wake: peer("b") });
    }

    #[test]
    fn test_build_agent_command_with_env() {
        let mut env = HashMap::new();
        env.insert("TEST_KEY".to_string(), "test_value".to_string());

        let config = HostConfig {
            vm_image: "test".into(),
            share_root: PathBuf::from("./test-shares"),
            rpc_port: 19384,
            surface_port: DEFAULT_SURFACE_PORT,
            host_ip: "192.168.64.1".into(),
            agent_binary_path: None,
            agent_env: env,
            agent_data_dir: None,
            memory_mb: None,
            cpu_count: None,
        };

        let cmd = build_agent_command(&config, None);
        assert!(cmd.contains("TEST_KEY"));
        assert!(cmd.contains("test_value"));
        assert!(cmd.contains("export"));
    }

    // -- SurfaceRouter: per-App surface frame routing (VC-P.1) ---------------

    /// [Verifies VC-P.1] Two Apps registered out of accept order still route
    /// correctly by App identity: a `SurfaceDrive` for App-A reaches App-A's
    /// client stub (not App-B's), and a `SurfaceObservation` from App-A's
    /// client reaches App-A's worker seam (not App-B's). Proves the router is
    /// keyed by App identity, not registration/accept order.
    #[tokio::test]
    async fn surface_router_routes_by_app_identity_not_accept_order() {
        use crate::agent::world::surface::{Hash, IdempotencyKey, Viewport, WindowState};
        use crate::protocol::{AgentToHost, HostToAgent, SurfaceObserved};

        let router = SurfaceRouter::new();

        // Register App-B before App-A — out of alphabetical / accept order —
        // to prove routing does not depend on registration sequence.
        let (drive_tx_b, mut drive_rx_b) = tokio::sync::mpsc::channel::<HostToAgent>(4);
        let (obs_tx_b, mut obs_rx_b) = tokio::sync::mpsc::channel::<AgentToHost>(4);
        router.register_client("app-b".into(), drive_tx_b).await;
        router.register_worker("app-b".into(), obs_tx_b).await;

        let (drive_tx_a, mut drive_rx_a) = tokio::sync::mpsc::channel::<HostToAgent>(4);
        let (obs_tx_a, mut obs_rx_a) = tokio::sync::mpsc::channel::<AgentToHost>(4);
        router.register_client("app-a".into(), drive_tx_a).await;
        router.register_worker("app-a".into(), obs_tx_a).await;

        // A SurfaceDrive for App-A must reach App-A's client stub only.
        let drive_frame = HostToAgent::SurfaceDrive {
            ops: vec![],
            cmd: 1,
            key: IdempotencyKey("cmd-1".into()),
        };
        assert!(
            router.route_drive("app-a", drive_frame).await,
            "route_drive must report delivery to App-A"
        );
        assert!(
            drive_rx_a.recv().await.is_some(),
            "SurfaceDrive must reach App-A's client stub"
        );
        assert!(
            drive_rx_b.try_recv().is_err(),
            "SurfaceDrive for App-A must NOT reach App-B's client stub"
        );

        // A SurfaceObservation from App-A's client must reach App-A's worker
        // seam only, not App-B's.
        let obs_frame = AgentToHost::SurfaceObservation {
            observed: SurfaceObserved {
                surface: 1,
                version: 0,
                ax_digest: Hash("digest-a".into()),
                focus: None,
                selection: None,
                viewport: Viewport(String::new()),
                window: WindowState(String::new()),
                cursor: None,
            },
        };
        assert!(
            router.route_inbound("app-a", obs_frame).await,
            "route_inbound must report delivery to App-A's worker seam"
        );
        assert!(
            obs_rx_a.recv().await.is_some(),
            "SurfaceObservation must reach App-A's worker seam"
        );
        assert!(
            obs_rx_b.try_recv().is_err(),
            "SurfaceObservation for App-A must NOT reach App-B's worker seam"
        );
    }
}
