//! effects — see docs/agent/world/ecs-runtime.md
//!
//! The P0 `Command` subset plus the imperative-shell DRIVERS that turn a tick's
//! emitted Commands into result Inputs. There are TWO drivers, separate code
//! paths rather than a flag inside a System (Inv 6):
//!
//! - the **live driver** performs the effect, APPENDS the result `Event` to the
//!   log BEFORE feeding it back (log-before-apply, Inv 4), and
//! - the **replay driver** DISCARDS the emitted Commands and stands in the
//!   already-logged result, reusing it ONLY while the re-emitted request
//!   re-hashes to the recorded `Fingerprint` (content-addressed replay, Inv 7).
//!
//! A System cannot tell which driver it runs under: replay re-calls nothing
//! because the replay driver suppresses every Command — not because any System
//! behaves differently. P0 implements ONLY the `CallModel` effect.

use std::collections::BTreeSet;
use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use super::autonomy::{AgentInteraction, HumanAction, Notify};
use super::blob::BlobStore;
use super::event_log::EventLog;
use super::history::{BlobHash, Block, History, ImageSource, Msg, ToolSchema};
use super::inputs::{
    CancelReason, DeliveryOutcome, Event, Fingerprint, LogicalInput, ModelError, Origin,
    PeerPayload,
};
use super::lifecycle::{ActorCtx, AppId, EffectId, EffectKind, IdempotencyKey, LifecycleEvent};
use super::model_bridge::{from_model_response, to_model_request};
use super::surface::{
    PeerEnvelopeId, SurfaceOp, fingerprint_ui_request, perceived_for_ops, set_value_tool_schema,
    surface_tool_result,
};
use super::world::{
    Activity, ByteCap, CmdId, EdgeId, EntityId, ModelConfig, PeerId, ReqId, SlotKind, SlotState,
    Tick, Timestamp, World,
};
use crate::error::Error;

/// A tool name, matching the `name` field of a `Block::ToolUse` block.
/// Aliased so Command fields read as domain concepts (the tool being run)
/// rather than the underlying `String` type.
pub type ToolName = String;

// ---------------------------------------------------------------------------
// Command (World → shell) — P0 subset
// ---------------------------------------------------------------------------

/// Commands a tick emits for the imperative shell to dispatch (P0 + P1a).
///
/// A Command is an INTENT to perform an effect. The LIVE driver dispatches it and
/// feeds the result back as a recorded Input; the REPLAY driver discards it and
/// stands in the already-logged result (the two-driver model). Each effectful
/// Command has a terminating Input dual (see docs/agent/world/ecs-runtime.md —
/// Command↔Input duals). Cancel/abort Commands need no `key`; aborting an already-
/// aborted effect is inherently idempotent. `RaiseInteraction` is deduped by its
/// stable `request_id` and also needs no key.
/// See docs/agent/world/ecs-runtime.md (Commands).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Dispatch one inference: assemble the `/v1/messages` body from
    /// `messages`/`params`, call the provider, and record the result as
    /// `ModelResponded` (or `ModelFailed`) correlated by `cmd`. `key` exists so a
    /// crash-resume re-dispatch can be deduped downstream (the full
    /// `IdempotencyKey` is the WAL milestone, P2). Dual: `ModelResponded` /
    /// `ModelFailed` / `InferenceCancelled`.
    CallModel {
        cmd: CmdId,
        entity: EntityId,
        messages: History,
        tools: ToolSet,
        params: ModelConfig,
        key: CommandKey,
    },

    /// Request cancellation of an in-flight `CallModel`. No `key` — aborting an
    /// already-cancelled inference is inherently idempotent.
    /// Dual: `InferenceCancelled`.
    CancelInference { cmd: CmdId },

    /// Dispatch one tool call. `key` enables crash-resume dedup.
    /// Dual: `ToolReturned`.
    RunTool {
        cmd: CmdId,
        entity: EntityId,
        tool: ToolName,
        args: Json,
        key: CommandKey,
    },

    /// Request cancellation of an in-flight `RunTool`. No `key` — idempotent.
    /// Dual: `ToolAborted`.
    CancelTool { cmd: CmdId },

    /// Ask a human to perform an action (e.g. provide text input). `notify`
    /// controls how the shell surfaces the request. `key` enables crash-resume
    /// dedup. Dual: `HumanActionDone`.
    RequestHumanAction {
        cmd: CmdId,
        entity: EntityId,
        ask: HumanAction,
        notify: Notify,
        key: CommandKey,
    },

    /// Abort a pending `RequestHumanAction`. No `key` — idempotent.
    /// Dual: `HumanActionAborted`.
    AbortHumanAction { cmd: CmdId },

    /// Send a message or drive command to a peer App process. The sender's
    /// local delivery ack is recorded as `PeerSendOutcome` — the ONLY dual of
    /// this Command. The receiver-side `DriveRequested`/`PeerDelivered` are
    /// EXOGENOUS inputs on the OTHER World, not duals of this Command (the
    /// per-`cmd` fingerprint does not cross the process boundary — see Theme 4b).
    /// `key` enables crash-resume dedup; the downstream dedupes by key on
    /// re-dispatch (effectively-once = at-least-once + idempotency).
    /// Dual: `PeerSendOutcome`.
    /// See docs/agent/world/ecs-runtime.md §645.
    SendPeer {
        cmd: CmdId,
        to: PeerId,
        payload: PeerPayload,
        key: CommandKey,
    },

    /// Raise an agent-facing interaction (e.g. an approval dialog). Deduped by
    /// the stable `request_id` so no separate `key` is needed.
    /// Dual: `InteractionAnswer`.
    RaiseInteraction {
        request_id: ReqId,
        entity: EntityId,
        interaction: AgentInteraction,
    },

    /// Summarize the oldest `upto` History messages into one compact message so
    /// the next request's context window shrinks (Boundedness). A model call;
    /// `key` enables crash-resume dedup. `messages` is the history slice to
    /// summarize (the oldest `upto` Msgs); `params` is the model config for the
    /// summarization call. The fingerprint is content-addressed over `(messages,
    /// params)` like `CallModel` (Inv 7). Dual: `Compacted` / `ModelFailed`.
    Compact {
        cmd: CmdId,
        entity: EntityId,
        upto: u32,
        /// The oldest `upto` History messages to summarize.
        messages: History,
        /// Model config for the summarization call.
        params: ModelConfig,
        key: CommandKey,
    },

    /// Carry a stratum-2 [`LifecycleEvent`] OBSERVABILITY record out of a pure
    /// System to the driver, which is the sole appender of lifecycle records. It
    /// is NOT an effect: it dispatches nothing, consumes no idempotency
    /// key/`effect_id`, and has no Input dual. The LIVE driver appends it via
    /// `append_lifecycle`; the REPLAY driver DISCARDS it (it is stratum-2 and
    /// neutral on replay — any World change it reports already lives in the
    /// stratum-1 fold, so replay stays byte-identical). Today this carries the
    /// Inbox-overflow `MessageDropped` notice SteeringSystem would otherwise have
    /// no channel to surface. See docs/agent/world/ecs-runtime.md (Stratum 2;
    /// Bounded queues — Inbox DropOldest).
    EmitLifecycle(LifecycleEvent),
}

/// The tools offered to a model call. P0 has no tools (the tool effect is a later
/// milestone), so this is an ordered list of tool names — empty in P0 — carried
/// so the request `Fingerprint` covers tool availability. The full structured
/// tool set arrives with the tool milestone. See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSet(pub Vec<String>);

/// P0 placeholder for an effectful Command's idempotency key. The full
/// `IdempotencyKey { app_id, tick, effect_id }` — which dedups a crash-resume
/// re-dispatch — arrives with the WAL milestone (P2). P0 carries this opaque
/// placeholder so the `Command` shape is stable without pulling WAL concerns in.
/// See docs/agent/world/ecs-runtime.md.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandKey;

// ---------------------------------------------------------------------------
// Fingerprint — canonical hash of a CallModel request payload
// ---------------------------------------------------------------------------

/// Compute the canonical request `Fingerprint` for a `CallModel`: a SHA-256 over
/// the deterministically-serialized `(messages, tools, params)` payload. Field
/// order is fixed and the payload carries no map-by-iteration or float
/// nondeterminism, so an identical request hashes identically across processes
/// and runs — the basis of content-addressed replay (Inv 7). Recorded on the
/// result and re-checked on re-emit. See docs/agent/world/ecs-runtime.md
/// (Content-addressed replay — the fingerprint).
pub fn fingerprint_call(
    messages: &History,
    tools: &ToolSet,
    params: &ModelConfig,
) -> Result<Fingerprint, Error> {
    /// The exact, fixed-order payload that is hashed. A dedicated struct so the
    /// field order is explicit rather than positional.
    #[derive(Serialize)]
    struct Payload<'a> {
        messages: &'a History,
        tools: &'a ToolSet,
        params: &'a ModelConfig,
    }

    let bytes = serde_json::to_vec(&Payload {
        messages,
        tools,
        params,
    })?;
    let digest = Sha256::digest(&bytes);
    Ok(Fingerprint(hex::encode(digest)))
}

/// Compute the canonical request `Fingerprint` for a `Compact`: a SHA-256 over
/// the deterministically-serialized `(messages, params)` payload of the compaction
/// call — the history slice being summarized and the model config used. The same
/// content-addressed discipline as `fingerprint_call` ensures an identical
/// compaction request hashes identically across processes and runs (Inv 7).
/// See docs/agent/world/ecs-runtime.md (Content-addressed replay; Compact↔Compacted).
pub fn fingerprint_compact(
    messages: &History,
    params: &ModelConfig,
) -> Result<Fingerprint, Error> {
    /// The exact, fixed-order payload that is hashed.
    #[derive(Serialize)]
    struct Payload<'a> {
        messages: &'a History,
        params: &'a ModelConfig,
    }

    let bytes = serde_json::to_vec(&Payload { messages, params })?;
    let digest = Sha256::digest(&bytes);
    Ok(Fingerprint(hex::encode(digest)))
}

/// Compute the canonical request `Fingerprint` for a `SendPeer`: a SHA-256 over the
/// deterministically-serialized `(to, payload)` of the outbound drive — the peer
/// address and the cross-World payload. The same content-addressed discipline as
/// `fingerprint_call`/`fingerprint_compact`: an identical send hashes identically
/// across processes and runs, so the recorded `PeerSendOutcome` is reused on replay
/// ONLY while the re-emitted send re-hashes to it (Inv 7), and the value doubles as
/// the sender-assigned, retry-stable delivery envelope id (effectively-once =
/// at-least-once + idempotency-by-envelope-id). See docs/agent/world/ecs-runtime.md
/// (Content-addressed replay; SendPeer↔PeerSendOutcome; Theme 4b).
pub fn fingerprint_peer(to: &PeerId, payload: &PeerPayload) -> Result<Fingerprint, Error> {
    /// The exact, fixed-order payload that is hashed.
    #[derive(Serialize)]
    struct Payload<'a> {
        to: &'a PeerId,
        payload: &'a PeerPayload,
    }

    let bytes = serde_json::to_vec(&Payload { to, payload })?;
    let digest = Sha256::digest(&bytes);
    Ok(Fingerprint(hex::encode(digest)))
}

// ---------------------------------------------------------------------------
// Tool resolution — a CallModel's ToolSet names → their Anthropic declarations
// ---------------------------------------------------------------------------

/// Resolve the tool NAMES a `CallModel` carries (`ToolSet`) to their Anthropic
/// `ToolSchema` declarations, so the assembled request body tells the model which
/// tools EXIST and how to shape each `tool_use.input`. YAGNI — a small static
/// name→schema table, not a registry: the only surface tool declared today is
/// `set_value`, whose `input_schema` matches the live `set_value` executor's
/// `args` shape. An unknown name resolves to nothing (it was carried for the
/// fingerprint only). See docs/agent/world/ecs-runtime.md.
fn resolve_tools(tools: &ToolSet) -> Vec<ToolSchema> {
    tools
        .0
        .iter()
        .filter_map(|name| match name.as_str() {
            "set_value" => Some(set_value_tool_schema()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Model-call capability — the config-selected provider::ModelApi
// ---------------------------------------------------------------------------
//
// The LIVE driver drives each `CallModel`/`Compact` through the config-selected
// [`crate::provider::ModelApi`] (built by `provider::selected_from_env()`), passed
// in as `&dyn ModelApi`. The world↔neutral conversion at this seam lives in
// [`super::model_bridge`] (so the provider stays free of any world import). The
// REPLAY driver takes NO model client — it cannot make a call by construction
// (Inv 6). See docs/agent/world/ecs-runtime.md.

// ---------------------------------------------------------------------------
// SurfaceDriver — the surface-drive sink the LIVE driver needs (stratum-2/live)
// ---------------------------------------------------------------------------

/// The surface-drive capability the LIVE driver depends on, abstracted behind a
/// trait so a test can inject a capturing stand-in without the macOS app, exactly
/// as the model-call seam (`provider::ModelApi`) abstracts the model call. When the live driver executes a
/// `set_value` surface `RunTool` it hands the Command to `drive`, whose production
/// impl converts it to a `HostToAgent::SurfaceDrive` frame (the worker's
/// `surface_drive_frame`) and forwards it to the macOS app so the screen actually
/// changes. The send is a stratum-2/LIVE-only effect — the REPLAY driver takes NO
/// `SurfaceDriver` and never re-sends it (the recorded `ToolReturned` already
/// carries the projection the fold reproduces). See docs/agent/world/ecs-runtime.md
/// (Theme 2a; SurfaceDrive; the agent UI-write echo is one fact).
pub trait SurfaceDriver {
    /// Forward a `set_value` surface `RunTool` out to the macOS app as a surface
    /// drive. Mirrors the model-call seam: the live driver awaits it as the real
    /// effect; a genuine forwarding failure propagates so the driver can record it.
    fn drive(&self, command: &Command) -> impl Future<Output = Result<(), Error>> + Send;
}

// ---------------------------------------------------------------------------
// PeerSender — the peer-drive sink the LIVE driver needs (stratum-2/live)
// ---------------------------------------------------------------------------

/// The peer-send capability the LIVE driver depends on, abstracted behind a trait
/// so a test can inject a stand-in without the shell `PeerBroker`, exactly as
/// the model-call seam (`provider::ModelApi`) abstracts the model call and `SurfaceDriver` the surface drive.
/// When the live driver dispatches a `Command::SendPeer` it hands the destination
/// `to`, the `payload`, the idempotency `key`, and the sender-assigned `envelope`
/// to `send`, whose production impl wraps the shell `PeerBroker::relay`, mapping its
/// `PeerRouteOutcome::{Delivered,Queued,Rejected}` to the `DeliveryOutcome` recorded
/// in `PeerSendOutcome`. The send is a stratum-2/LIVE-only effect — the REPLAY
/// driver takes NO `PeerSender` and never re-sends it (the recorded `PeerSendOutcome`
/// already carries the outcome the fold reproduces). The real broker-backed impl is
/// wired by the worker; here the trait is the seam. See
/// docs/agent/world/ecs-runtime.md (§645; Theme 4b; the World↔broker boundary).
pub trait PeerSender {
    /// Route one outbound peer drive/message to the broker and return its delivery
    /// `outcome`. Mirrors `SurfaceDriver::drive`: the live driver awaits it as the
    /// real effect; a genuine routing failure propagates so the driver can record it.
    fn send(
        &self,
        to: &PeerId,
        payload: &PeerPayload,
        key: &CommandKey,
        envelope: &PeerEnvelopeId,
    ) -> impl Future<Output = Result<DeliveryOutcome, Error>> + Send;
}

/// The placeholder `PeerSender` the production live driver routes `SendPeer` through
/// until the worker wires the real `PeerBroker`-backed sender. It performs no
/// delivery and reports every send `Queued` — parked for a peer not yet routable —
/// so a `SendPeer` settles its `Peer` slot without error rather than stalling the
/// tick. It is inert in practice: no production model is yet told the `drive_peer`
/// tool exists, so no `SendPeer` is emitted to reach it. The worker replaces this
/// with the broker wrapper that performs real, durable delivery. See
/// docs/agent/world/ecs-runtime.md (the World↔broker boundary).
pub struct UnattachedPeerSender;

impl PeerSender for UnattachedPeerSender {
    async fn send(
        &self,
        _to: &PeerId,
        _payload: &PeerPayload,
        _key: &CommandKey,
        _envelope: &PeerEnvelopeId,
    ) -> Result<DeliveryOutcome, Error> {
        Ok(DeliveryOutcome::Queued)
    }
}

// ---------------------------------------------------------------------------
// BlobSink — the externalize/resolve seam the LIVE driver needs (shell IO)
// ---------------------------------------------------------------------------

/// The content-addressed blob capability the LIVE driver depends on, abstracted
/// behind a trait so the externalize/resolve IO stays in the imperative SHELL and
/// out of the pure tick — exactly as the model-call seam/`SurfaceDriver`/`PeerSender`
/// abstract their effects. `BlobStore` (blob.rs) is the production implementation.
/// The externalize DECISION ([`over_inline_cap`]) is a pure function of
/// `(bytes.len(), cap)`; only `put`/`get` are this seam's shell effects, so a pure
/// System never touches blob IO. See docs/agent/world/ecs-runtime.md §1415-1434
/// (content-addressed durable blob store = PRIMARY STORAGE, not a derived cache).
pub trait BlobSink {
    /// Store `bytes` durably and return their content address — the shell side of
    /// externalize. Idempotent by content hash (identical bytes store once).
    fn put(&self, bytes: &[u8]) -> Result<BlobHash, Error>;
    /// Read the bytes stored at content address `hash` — the shell side of
    /// resolve, used to rebuild a request body from a `Blob{hash}` History entry.
    fn get(&self, hash: &BlobHash) -> Result<Vec<u8>, Error>;
}

/// The production `BlobSink`: the durable, content-addressed `BlobStore`. The
/// trait is the seam a test (or the inert default) stands in for; this impl is
/// what the worker threads once a session's blob store is attached (BL-sink).
impl BlobSink for BlobStore {
    fn put(&self, bytes: &[u8]) -> Result<BlobHash, Error> {
        BlobStore::put(self, bytes)
    }
    fn get(&self, hash: &BlobHash) -> Result<Vec<u8>, Error> {
        BlobStore::get(self, hash)
    }
}

/// The inert `BlobSink` the default [`drive_live`] threads until a real
/// `BlobStore` seam is attached: it externalizes/resolves NOTHING. It is never
/// reached while `Caps.blob_inline_cap == 0` (the default — no payload is ever
/// over an unbounded cap, and a cap-0 session records no `Blob{hash}` to resolve),
/// so the M3 driver path stays byte-identical. Engaging the externalize policy (a
/// positive cap) requires threading a real `BlobStore` (BL-sink); until then this
/// reports a clear error rather than silently dropping bytes. Mirrors
/// `UnattachedPeerSender`. See docs/agent/world/ecs-runtime.md §1415-1434.
pub struct UnattachedBlobSink;

impl BlobSink for UnattachedBlobSink {
    fn put(&self, _bytes: &[u8]) -> Result<BlobHash, Error> {
        Err(Error::World(
            "no blob store attached: externalizing an over-cap payload requires a \
             BlobStore seam (keep Caps.blob_inline_cap = 0 to leave payloads inline)"
                .into(),
        ))
    }
    fn get(&self, _hash: &BlobHash) -> Result<Vec<u8>, Error> {
        Err(Error::World(
            "no blob store attached: resolving a Blob{hash} requires a BlobStore seam".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Blob externalize / resolve — the pure decision + the shell put/get
// ---------------------------------------------------------------------------

/// The PURE inline-vs-externalize decision: a payload of `len` bytes is
/// externalized to the blob store when it EXCEEDS `cap`, and stays inline at or
/// under the cap. `cap == 0` means **unbounded** — never externalize. A pure
/// function of `(len, cap)`, so the policy is unit-testable without any IO; the
/// actual `put`/`get` are the shell's job. See docs/agent/world/ecs-runtime.md
/// §1415-1434 (two-stage blob design).
fn over_inline_cap(len: usize, cap: ByteCap) -> bool {
    cap != 0 && len > cap as usize
}

/// Externalize every over-cap inline image in `blocks` to `sink`, recording only
/// its content-address `Blob{hash}` — the log/snapshot then carry the hash, never
/// the bytes. A payload at/under the cap (or `cap == 0`) is left `Inline`
/// untouched, so a no-image (or all-small) response externalizes to ITSELF and the
/// recorded Input is byte-identical. Recurses into `ToolResult` content so a
/// nested image is externalized too. The decision is the pure [`over_inline_cap`];
/// the `put` is the shell effect. See docs/agent/world/ecs-runtime.md §1415-1434.
fn externalize_blocks<B: BlobSink>(
    blocks: Vec<Block>,
    cap: ByteCap,
    sink: &B,
) -> Result<Vec<Block>, Error> {
    blocks
        .into_iter()
        .map(|block| externalize_block(block, cap, sink))
        .collect()
}

/// Externalize one block per [`externalize_blocks`].
fn externalize_block<B: BlobSink>(block: Block, cap: ByteCap, sink: &B) -> Result<Block, Error> {
    match block {
        Block::Image {
            source: ImageSource::Inline { mime, bytes },
        } if over_inline_cap(bytes.len(), cap) => {
            let hash = sink.put(&bytes)?;
            Ok(Block::Image {
                source: ImageSource::Blob { hash, mime },
            })
        }
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Ok(Block::ToolResult {
            tool_use_id,
            content: externalize_blocks(content, cap, sink)?,
            is_error,
        }),
        other => Ok(other),
    }
}

/// Resolve every externalized `Blob{hash}` image in `history` back to its `Inline`
/// bytes via `sink`, so a request body built from History carries the real bytes
/// the model needs. The request FINGERPRINT is computed over the UN-resolved
/// (Blob-form) History by the caller, so live and replay hash identically (Inv 7);
/// only the BODY is resolved. A History with no externalized image resolves to
/// itself, so a no-image request body is byte-identical to the pre-blob path.
/// Recurses into `ToolResult` content. See docs/agent/world/ecs-runtime.md §1415-1434.
fn resolve_history<B: BlobSink>(history: &History, sink: &B) -> Result<History, Error> {
    let messages = history
        .0
        .iter()
        .map(|msg| {
            Ok(Msg {
                role: msg.role,
                content: msg
                    .content
                    .iter()
                    .cloned()
                    .map(|block| resolve_block(block, sink))
                    .collect::<Result<Vec<_>, Error>>()?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(History(messages))
}

/// Resolve one block per [`resolve_history`].
fn resolve_block<B: BlobSink>(block: Block, sink: &B) -> Result<Block, Error> {
    match block {
        Block::Image {
            source: ImageSource::Blob { hash, mime },
        } => Ok(Block::Image {
            source: ImageSource::Inline {
                mime,
                bytes: sink.get(&hash)?,
            },
        }),
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Ok(Block::ToolResult {
            tool_use_id,
            content: content
                .into_iter()
                .map(|b| resolve_block(b, sink))
                .collect::<Result<Vec<_>, Error>>()?,
            is_error,
        }),
        other => Ok(other),
    }
}

// ---------------------------------------------------------------------------
// ResultStamp — the envelope facts the Command does not carry
// ---------------------------------------------------------------------------

/// The log-envelope facts a driver stamps onto a result `Event` that the
/// `Command` itself does not carry: the counterpart `edge` a CONVERSATION result
/// (the model call) routes to, the distinct `app_edge` an agent SURFACE-WRITE
/// result routes to, the logical `at` tick it is recorded at, and the `wall`-time
/// the shell observed when it appended. The tick driver (the caller) owns these;
/// the effects driver only needs them to wrap the `LogicalInput` it produces.
///
/// `edge` and `app_edge` are DISTINCT so the per-edge mode projection (Inv 19)
/// folds the human conversation and the agent's surface drive INDEPENDENTLY: an
/// agent `set_value` write recorded on `app_edge` makes `mode(app_edge)` fold
/// `Driven` (agent-only) even while the human conversation edge stays `Assisted`
/// (Human + Agent) — concurrent per-edge modes. See docs/agent/world/ecs-runtime.md
/// (Event envelope; Mode-as-projection).
#[derive(Debug, Clone, Copy)]
pub struct ResultStamp {
    /// The edge a CONVERSATION result (model call / compaction) routes to.
    pub edge: EdgeId,
    /// The DISTINCT edge an agent SURFACE-WRITE result (the `set_value`
    /// `ToolReturned`) routes to, so `mode(app_edge)` folds `Driven` while the
    /// human conversation edge is unaffected.
    pub app_edge: EdgeId,
    pub at: Tick,
    pub wall: Option<Timestamp>,
}

// ---------------------------------------------------------------------------
// LIVE driver — dispatch the effect, log-before-apply, feed the result back
// ---------------------------------------------------------------------------

/// The LIVE driver with the DEFAULT inert blob seam — the exact signature the M3
/// callers (the `WorldDriver`) drive unchanged. It delegates to
/// [`drive_live_with_blob_sink`] threading an [`UnattachedBlobSink`], so no payload
/// is externalized on this path (the externalize policy engages only once a real
/// [`BlobStore`] seam is wired, BL-sink). With `Caps.blob_inline_cap == 0` (the
/// default) the seam is never touched, so this is byte-identical to the pre-blob
/// driver. See docs/agent/world/ecs-runtime.md §1415-1434.
pub async fn drive_live<S, P, L>(
    commands: &[Command],
    stamp: ResultStamp,
    world: &World,
    client: &dyn crate::provider::ModelApi,
    surface_driver: &S,
    peer_sender: &P,
    log: &mut L,
) -> Result<Vec<Event>, Error>
where
    S: SurfaceDriver,
    P: PeerSender,
    L: EventLog,
{
    drive_live_with_blob_sink(
        commands,
        stamp,
        world,
        client,
        surface_driver,
        peer_sender,
        &UnattachedBlobSink,
        log,
    )
    .await
}

/// The LIVE driver: dispatch each emitted `CallModel`, performing the real model
/// call, and feed its recorded result back as the next Input.
///
/// For each `CallModel` it first WRITE-AHEADS a stratum-2
/// `CommandDispatched` dispatch-intent (intent before commitment, Inv 5) BEFORE
/// performing the effect, carrying the `ActorCtx` (the durable `cmd → ctx` index,
/// Theme 1b/Inv 17), the `IdempotencyKey = (app_id, tick, effect_id)`, and the
/// request `Fingerprint`. It then builds the neutral request at the world↔neutral
/// seam ([`super::model_bridge`]), drives it through the config-selected
/// [`crate::provider::ModelApi`], reshapes the response back into the recorded
/// `(Vec<Block>, ModelMeta)`, and builds the result
/// `LogicalInput` — `ModelResponded` on success, `ModelFailed` on a call error
/// (a failed call is a RECORDED Input that drives the failure edge, not a driver
/// error) — whose `entity`/`origin`/`edge` INHERIT from the dispatch-intent's
/// `ctx`. It APPENDS the wrapping `Event` to the stratum-1 log BEFORE returning
/// it (log-before-apply, Inv 4), so the result is durable before any System folds
/// it. Genuine infrastructure failures (body assembly, log append) propagate via
/// `?`.
///
/// `effect_id` is the intra-tick ORDINAL of an effectful Command within this
/// emitted list. The dispatch tick is `stamp.at`; the supervisor-owned `app_id`
/// is threaded by the resume path (a later milestone), so P2-wal records the
/// key shape with a default `AppId` placeholder.
///
/// P0 has no system-prompt assembly (a later concern), so the request is built
/// with an empty `system`.
///
/// A `set_value` surface `RunTool` executes here too: the live driver forwards a
/// surface drive out the injected `surface_driver` sink (the LIVE-only effect) and
/// appends a `ToolReturned` carrying the `{"surface_ops":[...]}` projection
/// envelope and a UI-request `Fingerprint` that folds the perceived surface state
/// (Inv 18). The fingerprint reads the World's perceived `SurfaceView`
/// (`world.resources.surfaces`) so a re-emit diverges exactly when the manipulated
/// surface changed (Theme 2b). The agent UI write's SOLE record is that one
/// `ToolReturned`; the drive is never re-sent on replay (the REPLAY driver takes no
/// `surface_driver`).
///
/// A `Command::SendPeer` is routed here too: the live driver dispatches the drive
/// through the injected `peer_sender` seam (the LIVE-only effect) and appends a
/// `PeerSendOutcome` carrying the returned `DeliveryOutcome`. Its `entity` is read
/// back from `world` — the SendPeer Command does NOT carry it (a send is addressed
/// by `cmd`/`to`/`payload`/`key`) — by the `Peer` slot the emitting tick opened
/// `Pending { cmd: Some(cmd) }`, so the recorded outcome routes back to settle that
/// slot. `world` is also the source of the perceived `SurfaceView` the set_value
/// fingerprint reads. See docs/agent/world/ecs-runtime.md (§645; Theme 4b).
// Each argument is a DISTINCT injected capability the live tick must not own
// (the World/commands to drive, the result stamp/log envelope, and the four IO
// seams `client`/`surface_driver`/`peer_sender`/`blob_sink`). They are threaded
// rather than bundled so each seam stays independently stubbable in a test, the
// same reason `drive_live` already sits at the limit; the `blob_sink` is the one
// added here. See docs/agent/world/ecs-runtime.md §1415-1434.
#[allow(clippy::too_many_arguments)]
pub async fn drive_live_with_blob_sink<S, P, B, L>(
    commands: &[Command],
    stamp: ResultStamp,
    world: &World,
    client: &dyn crate::provider::ModelApi,
    surface_driver: &S,
    peer_sender: &P,
    blob_sink: &B,
    log: &mut L,
) -> Result<Vec<Event>, Error>
where
    S: SurfaceDriver,
    P: PeerSender,
    B: BlobSink,
    L: EventLog,
{
    // The World's perceived surface view the UI-request fingerprint reads (Inv 18).
    let surfaces = &world.resources.surfaces;
    let mut results = Vec::with_capacity(commands.len());
    // The intra-tick effect ordinal: the n-th EFFECTFUL Command this tick. Reset
    // per `drive_live` call (one call drives one tick's Commands).
    let mut effect_id: EffectId = 0;
    for command in commands {
        match command {
            Command::CallModel {
                cmd,
                entity,
                messages,
                tools,
                params,
                ..
            } => {
                let fingerprint = fingerprint_call(messages, tools, params)?;
                // The durable cmd → ctx index: a model call serves the agent's turn
                // (`origin = Agent`) on the relationship the turn serves (`edge`).
                let ctx = ActorCtx {
                    entity: *entity,
                    origin: Origin::Agent,
                    edge: stamp.edge,
                };
                let key = IdempotencyKey {
                    app_id: AppId::default(),
                    tick: stamp.at,
                    effect_id,
                };
                // Write-ahead the dispatch-intent BEFORE acting (Inv 5). Neutral on
                // replay; read only by resume.
                let dispatched = LifecycleEvent::CommandDispatched {
                    at: stamp.at,
                    cmd: *cmd,
                    kind: EffectKind::CallModel,
                    ctx,
                    key,
                    fingerprint: fingerprint.clone(),
                };
                log.append_lifecycle(&dispatched)?;
                effect_id += 1;

                // Resolve the command's tool NAMES to their declarations so the
                // request tells the model which tools exist (an empty ToolSet
                // resolves to none → the body omits `tools`, byte-identical to a
                // tool-less call).
                let tool_schemas = resolve_tools(tools);
                // Resolve any externalized `Blob{hash}` image back to its bytes for
                // the request body the model receives. The request `fingerprint`
                // above is computed over the UN-resolved (Blob-form) `messages`, so
                // live and replay hash identically (Inv 7) — only the BODY carries
                // resolved bytes. A History with no externalized image resolves to
                // itself, so a no-image body is byte-identical to before. See
                // docs/agent/world/ecs-runtime.md §1415-1434.
                let resolved = resolve_history(messages, blob_sink)?;
                // Build the neutral request at the world↔neutral seam and drive it
                // through the config-selected provider; the returned neutral
                // `ModelResponse` is reshaped back into the exact `(Vec<Block>,
                // ModelMeta)` the former client returned, so the recorded result is
                // byte-identical (replay determinism). The fingerprint above is over
                // the UN-resolved world `messages`, untouched by this conversion.
                let request = to_model_request("", params, &tool_schemas, &resolved);
                // The result INHERITS its `entity` from the dispatch-intent's `ctx`
                // (Inv 17), not from a positional scan.
                let input = match client.turn(&request).await {
                    Ok(response) => {
                        let (blocks, meta) = from_model_response(response);
                        // Externalize any over-cap image the model returned to the
                        // blob store, recording only its `Blob{hash}` — the
                        // log/snapshot then carry the hash, never the bytes (the
                        // externalize is the SHELL's `put`, not a pure System). A
                        // no-image (or all-small) response externalizes to itself, so
                        // `ModelResponded` stays byte-identical for a no-image log.
                        // See docs/agent/world/ecs-runtime.md §1415-1434.
                        let blocks =
                            externalize_blocks(blocks, world.resources.caps.blob_inline_cap, blob_sink)?;
                        LogicalInput::ModelResponded {
                            cmd: *cmd,
                            entity: ctx.entity,
                            fingerprint,
                            blocks,
                            meta,
                        }
                    }
                    Err(error) => LogicalInput::ModelFailed {
                        cmd: *cmd,
                        entity: ctx.entity,
                        fingerprint,
                        error: classify(&error),
                    },
                };
                // The wrapping Event inherits `origin`/`edge` from the same `ctx`.
                let event = Event {
                    origin: ctx.origin,
                    edge: ctx.edge,
                    at: stamp.at,
                    wall: stamp.wall,
                    input,
                };
                // Log-before-apply (Inv 4): durable BEFORE it is fed back.
                log.append(&event)?;
                results.push(event);
            }
            // A `set_value` surface `RunTool` is the one tool with a LIVE executor in
            // this milestone: it (1) forwards a surface drive out the injected sink so
            // the imperative shell applies it to the macOS app, and (2) records the
            // agent UI write as its SOLE fact — a `ToolReturned` carrying the
            // `{"surface_ops":[...]}` projection envelope and a UI-request
            // `Fingerprint` that folds the perceived surface state (Inv 18). Other
            // tools' live drivers arrive with their own milestones; they still consume
            // an `effect_id` ordinal so a later effectful Command keeps a stable
            // intra-tick ordinal once their drivers land. See
            // docs/agent/world/ecs-runtime.md (The agent UI-write echo is one fact).
            Command::RunTool {
                cmd,
                entity,
                tool,
                args,
                ..
            } if tool == "set_value" => {
                // The canonical `set_value` encoding carries its ops in
                // `args["surface_ops"]` (matching the SurfaceDrive frame converter);
                // a missing/malformed field decodes to no ops (a no-op drive).
                let ops: Vec<SurfaceOp> = args
                    .get("surface_ops")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                // The UI-request Fingerprint folds the perceived state of the surface
                // the ops target (Inv 18 / Theme 2b): a re-emit diverges exactly when
                // that surface changed.
                let perceived = perceived_for_ops(surfaces, &ops);
                let fingerprint = fingerprint_ui_request(tool, args, &perceived)?;
                // The durable cmd → ctx index: a surface write serves the agent's turn
                // (`origin = Agent`) on the DISTINCT APP edge (`stamp.app_edge`), NOT
                // the human conversation `edge`, so `mode(app_edge)` folds `Driven`
                // (agent-only) while the conversation edge stays unaffected (GAP B /
                // Inv 19). See docs/agent/world/ecs-runtime.md (Mode-as-projection).
                let ctx = ActorCtx {
                    entity: *entity,
                    origin: Origin::Agent,
                    edge: stamp.app_edge,
                };
                let key = IdempotencyKey {
                    app_id: AppId::default(),
                    tick: stamp.at,
                    effect_id,
                };
                // Write-ahead the dispatch-intent BEFORE acting (Inv 5). Neutral on
                // replay; read only by resume.
                let dispatched = LifecycleEvent::CommandDispatched {
                    at: stamp.at,
                    cmd: *cmd,
                    kind: EffectKind::RunTool,
                    ctx,
                    key,
                    fingerprint: fingerprint.clone(),
                };
                log.append_lifecycle(&dispatched)?;
                effect_id += 1;

                // The LIVE effect (stratum-2/live-only): forward the drive so the
                // shell applies it to the macOS app. Replay NEVER re-sends it — the
                // recorded `ToolReturned` already carries the projection the fold
                // reproduces.
                surface_driver.drive(command).await?;

                // The agent UI write's SOLE record: a `ToolReturned` carrying the
                // projection envelope SurfaceSystem folds ONCE into a version bump —
                // or an `is_error` result when an op was rejected by the optimistic-
                // concurrency precondition. `tool_use_id` is empty here; the owning
                // ToolSystem re-stamps it to the slot's id when it settles.
                let result = vec![surface_tool_result("", surfaces, &ops)?];
                let input = LogicalInput::ToolReturned {
                    cmd: *cmd,
                    entity: ctx.entity,
                    fingerprint,
                    result,
                };
                // The wrapping Event inherits `origin`/`edge` from the same `ctx`.
                let event = Event {
                    origin: ctx.origin,
                    edge: ctx.edge,
                    at: stamp.at,
                    wall: stamp.wall,
                    input,
                };
                // Log-before-apply (Inv 4): durable BEFORE it is fed back.
                log.append(&event)?;
                results.push(event);
            }
            // A `Command::SendPeer` routes one outbound peer drive/message to the
            // shell broker via the injected `peer_sender` seam and records its
            // delivery `outcome` as a `PeerSendOutcome` — the SOLE dual of `SendPeer`
            // (Theme 4b). It uses the same write-ahead / log-before-apply / content-
            // addressed-fingerprint discipline as `CallModel`; the receiver-side
            // `DriveRequested`/`PeerDelivered` are EXOGENOUS inputs on the OTHER World,
            // not produced here. See docs/agent/world/ecs-runtime.md (§645; Theme 4b).
            Command::SendPeer {
                cmd,
                to,
                payload,
                key,
            } => {
                // Content-addressed over (to, payload): a re-emit re-hashes
                // identically so replay reuses the recorded `PeerSendOutcome` (Inv 7).
                let fingerprint = fingerprint_peer(to, payload)?;
                // The SendPeer Command does not carry its `entity` (a send is
                // addressed by cmd/to/payload/key); recover it from the `Peer` slot
                // the emitting tick opened `Pending { cmd: Some(cmd) }` so the recorded
                // outcome routes back to settle that slot (Inv 16).
                let entity = entity_owning_peer_cmd(world, *cmd);
                // The durable cmd → ctx index: a peer send serves the agent's turn
                // (`origin = Agent`) on the relationship the turn serves (`edge`).
                let ctx = ActorCtx {
                    entity,
                    origin: Origin::Agent,
                    edge: stamp.edge,
                };
                let key_id = IdempotencyKey {
                    app_id: AppId::default(),
                    tick: stamp.at,
                    effect_id,
                };
                // Write-ahead the dispatch-intent BEFORE acting (Inv 5). Neutral on
                // replay; read only by resume, which re-dispatches `SendPeer` under
                // the SAME key (the downstream dedupes — effectively-once).
                let dispatched = LifecycleEvent::CommandDispatched {
                    at: stamp.at,
                    cmd: *cmd,
                    kind: EffectKind::SendPeer,
                    ctx,
                    key: key_id,
                    fingerprint: fingerprint.clone(),
                };
                log.append_lifecycle(&dispatched)?;
                effect_id += 1;

                // The sender-assigned, retry-stable delivery envelope id: a crash-
                // resume re-dispatch of the SAME send carries the SAME id, so the
                // receiver dedupes a redelivery (idempotency-by-envelope-id). Content-
                // addressed over the send, like the fingerprint.
                let envelope = PeerEnvelopeId(fingerprint.0.clone());

                // The LIVE effect (stratum-2/live-only): route the drive to the broker.
                // Replay NEVER re-sends it — the recorded `PeerSendOutcome` already
                // carries the outcome the fold reproduces.
                let outcome = peer_sender.send(to, payload, key, &envelope).await?;

                let input = LogicalInput::PeerSendOutcome {
                    cmd: *cmd,
                    entity: ctx.entity,
                    fingerprint,
                    to: to.clone(),
                    outcome,
                };
                // The wrapping Event inherits `origin`/`edge` from the same `ctx`.
                let event = Event {
                    origin: ctx.origin,
                    edge: ctx.edge,
                    at: stamp.at,
                    wall: stamp.wall,
                    input,
                };
                // Log-before-apply (Inv 4): durable BEFORE it is fed back.
                log.append(&event)?;
                results.push(event);
            }
            // Other EFFECTFUL Commands (live dispatch arrives in later milestones)
            // still CONSUME an `effect_id` ordinal, so a `CallModel` emitted after
            // them keeps a stable intra-tick ordinal once their drivers land.
            Command::RunTool { .. } | Command::RequestHumanAction { .. } => {
                effect_id += 1;
            }
            // `Compact` is a model call that summarizes the oldest `upto` History
            // messages; it uses the same write-ahead / log-before-apply / fingerprint
            // discipline as `CallModel`. On success it produces `Compacted`; on
            // failure `ModelFailed` (best-effort — compaction failure is not a sink).
            // See docs/agent/world/ecs-runtime.md (Context-window compaction; Compact↔Compacted).
            Command::Compact {
                cmd,
                entity,
                messages,
                params,
                ..
            } => {
                let fingerprint = fingerprint_compact(messages, params)?;
                let ctx = ActorCtx {
                    entity: *entity,
                    origin: Origin::Agent,
                    edge: stamp.edge,
                };
                let key = IdempotencyKey {
                    app_id: AppId::default(),
                    tick: stamp.at,
                    effect_id,
                };
                // Write-ahead the dispatch-intent BEFORE acting (Inv 5).
                let dispatched = LifecycleEvent::CommandDispatched {
                    at: stamp.at,
                    cmd: *cmd,
                    kind: EffectKind::Compact,
                    ctx,
                    key,
                    fingerprint: fingerprint.clone(),
                };
                log.append_lifecycle(&dispatched)?;
                effect_id += 1;

                // Build the summarization request: system prompt + the history
                // slice to condense. Resolve any externalized `Blob{hash}` image to
                // its bytes for the body (the `fingerprint` above stays over the
                // Blob-form `messages`, so live and replay hash identically, Inv 7).
                // The model returns the summary blocks.
                let resolved = resolve_history(messages, blob_sink)?;
                // The summarization request, built at the world↔neutral seam (no
                // tools) and driven through the config-selected provider; the
                // fingerprint above stays over the Blob-form `messages`.
                let request = to_model_request(
                    "Summarize the following conversation into a single concise \
                     message that preserves all key information and context \
                     needed to continue the conversation coherently.",
                    params,
                    &[],
                    &resolved,
                );
                let input = match client.turn(&request).await {
                    Ok(response) => {
                        let (blocks, _meta) = from_model_response(response);
                        // Externalize any over-cap image in the summary the same way
                        // as a `CallModel` response; a no-image summary is byte-identical.
                        let summary =
                            externalize_blocks(blocks, world.resources.caps.blob_inline_cap, blob_sink)?;
                        LogicalInput::Compacted {
                            cmd: *cmd,
                            entity: ctx.entity,
                            fingerprint,
                            summary,
                            replaced: messages.0.len() as u32,
                        }
                    }
                    Err(error) => LogicalInput::ModelFailed {
                        cmd: *cmd,
                        entity: ctx.entity,
                        fingerprint,
                        error: classify(&error),
                    },
                };
                let event = Event {
                    origin: ctx.origin,
                    edge: ctx.edge,
                    at: stamp.at,
                    wall: stamp.wall,
                    input,
                };
                // Log-before-apply (Inv 4): durable BEFORE it is fed back.
                log.append(&event)?;
                results.push(event);
            }
            // A stratum-2 observability record: APPEND it via `append_lifecycle`
            // (the driver is the sole lifecycle appender) and produce no stratum-1
            // result. It is not an effect, so it consumes no `effect_id` ordinal.
            // This is how a pure System's notice (e.g. the Inbox-overflow
            // `MessageDropped`) reaches the live log without folding into the World.
            Command::EmitLifecycle(event) => {
                log.append_lifecycle(event)?;
            }
            // Non-effectful Commands (the cancel/abort duals and `RaiseInteraction`)
            // carry no idempotency key and consume no ordinal.
            Command::CancelInference { .. }
            | Command::CancelTool { .. }
            | Command::AbortHumanAction { .. }
            | Command::RaiseInteraction { .. } => {}
        }
    }
    Ok(results)
}

/// Classify a model-call `Error` into the recorded `ModelError` that drives the
/// failure edge. The `TurnSystem` later refines transient-vs-terminal retry
/// policy from this value; the driver only records the failure shape.
fn classify(error: &Error) -> ModelError {
    match error {
        Error::ProviderApi { status, .. } => ModelError::Http(*status),
        Error::ProviderHttp(e) if e.is_timeout() => ModelError::Timeout,
        Error::ProviderHttp(e) => ModelError::Transport(e.to_string()),
        other => ModelError::Transport(other.to_string()),
    }
}

/// Recover the entity that owns an outbound `SendPeer` `cmd`: the entity whose
/// `ResolvingToolUses` turn holds a `Peer` slot `Pending { cmd: Some(cmd) }`. The
/// `SendPeer` Command does not carry its `entity` (a send is addressed by
/// `cmd`/`to`/`payload`/`key`), so the live driver reads it back from the World the
/// emitting tick left — where PeerDriveSystem recorded `cmd` in the slot (Inv 16) —
/// to route the resulting `PeerSendOutcome` to settle that exact slot. Falls back to
/// the World `root` if no slot matches, keeping the lookup total; in practice a
/// `SendPeer` is only ever emitted for an open `Peer` slot, so the scan finds it.
fn entity_owning_peer_cmd(world: &World, cmd: CmdId) -> EntityId {
    world
        .entities
        .iter()
        .find(|(_, components)| match &components.activity {
            Activity::ResolvingToolUses { slots } => slots.iter().any(|slot| {
                matches!(slot.kind, SlotKind::Peer)
                    && matches!(slot.state, SlotState::Pending { cmd: Some(c) } if c == cmd)
            }),
            _ => false,
        })
        .map(|(id, _)| *id)
        .unwrap_or(world.root)
}

// ---------------------------------------------------------------------------
// REPLAY driver — discard the Command, stand in the logged result
// ---------------------------------------------------------------------------

/// The outcome of standing in a single emitted `CallModel` during replay.
// Replay-critical control enum; boxing would touch drive_replay — suppress the size lint.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Replayed {
    /// The re-emitted request re-hashed to the recorded result's `Fingerprint` →
    /// reuse the recorded `Event` (the Command is suppressed). The default replay
    /// path on an unchanged run (Inv 7).
    Reused(Event),
    /// The re-emitted request DIVERGES from the recorded result (fingerprint
    /// mismatch), or the recorded results are exhausted → the fork/edit boundary.
    /// The caller MUST hand this Command (and every later one on the branch) to
    /// the LIVE driver. The replay driver NEVER goes live itself (it has no
    /// client). The transition is one-way: replay → live, exactly once. See
    /// docs/agent/world/ecs-runtime.md (One-way replay → live handoff).
    Diverged,
}

/// A read cursor over the DERIVED result `Event`s recorded in the log, advanced
/// as the replay driver reuses each one. EXOGENOUS inputs (session header, user
/// messages) are NOT here — the replay driver only stands in for effect results.
pub struct ReplayCursor {
    /// The recorded `ModelResponded`/`ModelFailed` Events, in log order.
    results: Vec<Event>,
    /// The next unreused result.
    pos: usize,
}

impl ReplayCursor {
    /// Build a cursor over the DERIVED result Events in `events` (the loaded log),
    /// preserving log order and skipping EXOGENOUS inputs.
    pub fn new(events: &[Event]) -> Self {
        let results = events
            .iter()
            .filter(|e| is_derived_result(&e.input))
            .cloned()
            .collect();
        ReplayCursor { results, pos: 0 }
    }

    /// Take the next recorded result, advancing the cursor.
    fn next_result(&mut self) -> Option<Event> {
        let event = self.results.get(self.pos).cloned();
        if event.is_some() {
            self.pos += 1;
        }
        event
    }
}

/// The REPLAY driver: DISCARD the emitted Commands and stand in the already-logged
/// result for each, reusing it ONLY while the re-emitted request re-hashes to the
/// recorded `Fingerprint`.
///
/// For each `CallModel` it recomputes the request `Fingerprint` and compares it to
/// the next recorded result's: a MATCH reuses that recorded `Event` (`Reused`); a
/// MISMATCH — or an exhausted log — is the fork/edit boundary (`Diverged`), at
/// which the run flips one-way to live and the caller takes over from this
/// Command onward. The driver makes NO model call (it takes no model client), so
/// replay reconstructs the World with zero model calls (Inv 6/7).
pub fn drive_replay(commands: &[Command], cursor: &mut ReplayCursor) -> Result<Vec<Replayed>, Error> {
    let mut out = Vec::with_capacity(commands.len());
    for command in commands {
        match command {
            Command::CallModel {
                messages,
                tools,
                params,
                ..
            } => {
                let emitted = fingerprint_call(messages, tools, params)?;
                match cursor.next_result() {
                    Some(event)
                        if recorded_fingerprint(&event.input) == Some(&emitted) =>
                    {
                        out.push(Replayed::Reused(event));
                    }
                    // Mismatch (stale recorded result) or exhausted log → boundary;
                    // the run goes live from here. One-way, so stop replaying.
                    _ => {
                        out.push(Replayed::Diverged);
                        break;
                    }
                }
            }
            // `Compact` is fingerprinted like `CallModel`: the replay driver
            // reuses the logged `Compacted` / `ModelFailed` only while the
            // re-emitted compaction request hashes identically to the recorded
            // result's fingerprint (content-addressed replay, Inv 7).
            Command::Compact { messages, params, .. } => {
                let emitted = fingerprint_compact(messages, params)?;
                match cursor.next_result() {
                    Some(event)
                        if recorded_fingerprint(&event.input) == Some(&emitted) =>
                    {
                        out.push(Replayed::Reused(event));
                    }
                    _ => {
                        out.push(Replayed::Diverged);
                        break;
                    }
                }
            }
            // `SendPeer` is fingerprinted like `CallModel`: the replay driver reuses
            // the logged `PeerSendOutcome` (the SOLE dual, Theme 4b) ONLY while the
            // re-emitted send hashes identically to the recorded outcome's fingerprint
            // (content-addressed replay, Inv 7) — so a recorded peer-drive branch
            // settles its `Peer` slot on replay with ZERO live sends.
            Command::SendPeer { to, payload, .. } => {
                let emitted = fingerprint_peer(to, payload)?;
                match cursor.next_result() {
                    Some(event)
                        if recorded_fingerprint(&event.input) == Some(&emitted) =>
                    {
                        out.push(Replayed::Reused(event));
                    }
                    _ => {
                        out.push(Replayed::Diverged);
                        break;
                    }
                }
            }
            // `EmitLifecycle` is a stratum-2 observability record, neutral on
            // replay: DISCARD it so the replay fold stays byte-identical (the
            // World change it reports is already reproduced by the stratum-1 fold).
            Command::EmitLifecycle(_) => {}
            // P1a Commands: replay drivers for these Commands arrive with their
            // respective milestones. The replay driver skips them here so it
            // remains compilable while Systems are added incrementally.
            Command::CancelInference { .. }
            | Command::RunTool { .. }
            | Command::CancelTool { .. }
            | Command::RequestHumanAction { .. }
            | Command::AbortHumanAction { .. }
            | Command::RaiseInteraction { .. } => {}
        }
    }
    Ok(out)
}

/// Whether a `LogicalInput` is a DERIVED effect result the replay driver stands
/// in for. Only fingerprinted DERIVED variants qualify — the named non-
/// fingerprinted exceptions (`ChildReturned`, `ToolAborted`, `HumanActionAborted`)
/// are in-World identity-correlated results, not shell-dispatched effect results.
/// `PeerSendOutcome` is DERIVED: it is the sender-local ack of `SendPeer`,
/// fingerprinted for content-addressed replay (Theme 4b).
fn is_derived_result(input: &LogicalInput) -> bool {
    matches!(
        input,
        LogicalInput::ModelResponded { .. }
            | LogicalInput::ModelFailed { .. }
            | LogicalInput::InferenceCancelled { .. }
            | LogicalInput::ToolReturned { .. }
            | LogicalInput::HumanActionDone { .. }
            | LogicalInput::Compacted { .. }
            | LogicalInput::PeerSendOutcome { .. }
    )
}

/// The recorded request `Fingerprint` a fingerprinted DERIVED result carries,
/// used to gate reuse against the freshly re-emitted request. Returns `None`
/// for EXOGENOUS inputs and the named non-fingerprinted DERIVED exceptions
/// (`ChildReturned`, `ToolAborted`, `HumanActionAborted`).
fn recorded_fingerprint(input: &LogicalInput) -> Option<&Fingerprint> {
    match input {
        LogicalInput::ModelResponded { fingerprint, .. }
        | LogicalInput::ModelFailed { fingerprint, .. }
        | LogicalInput::InferenceCancelled { fingerprint, .. }
        | LogicalInput::ToolReturned { fingerprint, .. }
        | LogicalInput::HumanActionDone { fingerprint, .. }
        | LogicalInput::Compacted { fingerprint, .. }
        | LogicalInput::PeerSendOutcome { fingerprint, .. } => Some(fingerprint),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// RESUME (crash recovery) — reconcile outstanding dispatch-intents on the
// replay → resume edge
// ---------------------------------------------------------------------------

/// The reconciliation of ONE outstanding `CommandDispatched` on resume — a
/// dispatch-intent whose effect was in flight when the App crashed (written, but
/// its result Input never logged). See docs/agent/world/ecs-runtime.md
/// (Reconciliation = the replay→resume edge; Inv 5).
#[derive(Debug, Clone, PartialEq)]
pub enum Reconciliation {
    /// A synthesised stratum-1 result was LOGGED (and is returned here): a
    /// `Thinking` crash's `InferenceCancelled { reason: Crash }`, or a
    /// non-idempotent effectful tool's `is_error` `ToolReturned`
    /// (surfaced-as-failed). The crash is a REAL logical Input — not a silent
    /// rewind — so the entity settles deterministically when the reducer folds
    /// it (Thinking → Idle), and replay reproduces it. After settling,
    /// Autonomy/budget decides a FRESH, accounted retry (new `cmd`, new `key`);
    /// that retry is named here, not performed.
    Settled(Event),

    /// An idempotent / key-deduped effect is safe to re-present: the SAME
    /// idempotency `key` is re-dispatched so the downstream dedupes the second
    /// attempt (effectively-once = at-least-once + idempotency). resume NAMES the
    /// re-dispatch by surfacing the outstanding dispatch's `cmd`/`kind`/`key`; the
    /// live driver reconstructs the request payload from the rebuilt World's tail
    /// Activity and re-emits under this key. See docs/agent/world/ecs-runtime.md
    /// (Reconciliation table — "re-dispatch (same key)").
    Redispatch {
        cmd: CmdId,
        kind: EffectKind,
        key: IdempotencyKey,
    },
}

/// The RESUME (live-recovery) phase of crash recovery, the second half of the
/// asymmetric replay → resume edge: after replay has rebuilt the World by folding
/// stratum-1 Inputs (suppressing every emitted Command), resume scans the
/// stratum-2 dispatch-intents for any `CommandDispatched` whose `cmd` has NO
/// matching terminal result in the stratum-1 tail — an effect that was in flight
/// at the crash (intent written before commitment, Inv 5) — and reconciles each
/// per its `EffectKind`:
///
/// - `CallModel` (a `Thinking` tail) → synthesise AND LOG a stratum-1
///   `InferenceCancelled { cmd, entity, fingerprint, partial: None, reason: Crash }`
///   so the entity settles. The crash's behavioral effect becomes a real logical
///   Input (not a neutral lifecycle trace), so budget/Autonomy see it and replay
///   reproduces it.
/// - `RunTool` (a non-idempotent effectful tool) → synthesise AND LOG a stratum-1
///   `ToolReturned` carrying an `is_error` `ToolResult` ("effect interrupted by
///   crash; not safely retryable"). Absent a recorded `ToolEffect` proving the
///   tool is Observational or Effectful·idempotent (that classification is a later
///   milestone), surfacing-as-failed is the only SAFE reconciliation — the design
///   rule is "re-run ONLY if idempotent, otherwise surface-as-failed".
/// - `RequestHumanAction` / `SendPeer` / `ScheduleTimer` / `Compact` → re-dispatch
///   with the SAME `key` (`Redispatch`); the downstream dedupes by key so the
///   effect commits effectively-once.
///
/// Each synthesised result fills `cmd`/`entity`/`fingerprint` from the outstanding
/// `CommandDispatched` and inherits its Event envelope `origin`/`edge` from the
/// recorded `ctx` (Inv 17), so it correlates by IDENTITY exactly like a real
/// result. Synthesised results are appended to the stratum-1 tail BEFORE being
/// returned (log-before-apply, Inv 4), each at its own fresh tick after the loaded
/// log (one Input per tick). `events`/`lifecycle` are the loaded stratum-1 /
/// stratum-2 streams; `log` is the same log they were loaded from.
pub fn resume<L>(
    events: &[Event],
    lifecycle: &[LifecycleEvent],
    log: &mut L,
) -> Result<Vec<Reconciliation>, Error>
where
    L: EventLog,
{
    // The `cmd`s that ALREADY have a terminal result in the stratum-1 tail. A
    // dispatch-intent whose `cmd` is absent here is an outstanding effect. A
    // deterministic `BTreeSet` (never a `HashMap`) keeps membership lookup free of
    // a hidden ordering input (Inv 8).
    let resolved: BTreeSet<CmdId> = events.iter().filter_map(|e| result_cmd(&e.input)).collect();

    // The resume phase appends synthesised results to the tail; each settles at its
    // own fresh tick after the loaded log (one Input per tick, Inv 12).
    let mut next_at: Tick = events.iter().map(|e| e.at).max().map_or(0, |m| m + 1);

    let mut out = Vec::new();
    for record in lifecycle {
        let LifecycleEvent::CommandDispatched {
            cmd,
            kind,
            ctx,
            key,
            fingerprint,
            ..
        } = record
        else {
            // The other stratum-2 records are behaviorally neutral trace; only a
            // dispatch-intent is load-bearing for recovery.
            continue;
        };
        // A dispatch whose result is already logged committed before the crash —
        // nothing to reconcile.
        if resolved.contains(cmd) {
            continue;
        }

        let outcome = reconcile(*cmd, *kind, *ctx, key.clone(), fingerprint.clone(), next_at);
        if let Reconciliation::Settled(event) = &outcome {
            // Log-before-apply (Inv 4): the synthesised Input is durable BEFORE any
            // System folds it.
            log.append(event)?;
            next_at += 1;
        }
        out.push(outcome);
    }
    Ok(out)
}

/// The per-`EffectKind` reconciliation policy for one outstanding dispatch — a
/// PURE function of the recorded dispatch facts and the resume tick `at`. The
/// caller logs a `Settled` event; this only constructs it. See
/// docs/agent/world/ecs-runtime.md (Reconciliation table).
fn reconcile(
    cmd: CmdId,
    kind: EffectKind,
    ctx: ActorCtx,
    key: IdempotencyKey,
    fingerprint: Fingerprint,
    at: Tick,
) -> Reconciliation {
    /// Wrap a synthesised `LogicalInput` in its Event envelope, inheriting
    /// `origin`/`edge` from the dispatch's `ctx` (Inv 17). `wall` is `None` — the
    /// crash moment was never observed, and a `None` wall leaves `Resources.wall`
    /// untouched when folded.
    fn settle(ctx: ActorCtx, at: Tick, input: LogicalInput) -> Reconciliation {
        Reconciliation::Settled(Event {
            origin: ctx.origin,
            edge: ctx.edge,
            at,
            wall: None,
            input,
        })
    }

    match kind {
        // A `Thinking` crash: the synthesised cancellation settles the entity.
        EffectKind::CallModel => settle(
            ctx,
            at,
            LogicalInput::InferenceCancelled {
                cmd,
                entity: ctx.entity,
                fingerprint,
                partial: None,
                reason: CancelReason::Crash,
            },
        ),
        // A non-idempotent effectful tool: surface-as-failed (the safe default —
        // idempotency is unprovable without a recorded `ToolEffect`). The slot's
        // owning System re-stamps the result's `tool_use_id`; the `is_error` flag
        // is what crosses, so the model sees a well-formed failed `tool_result`.
        EffectKind::RunTool => settle(
            ctx,
            at,
            LogicalInput::ToolReturned {
                cmd,
                entity: ctx.entity,
                fingerprint,
                result: vec![Block::ToolResult {
                    tool_use_id: String::new(),
                    content: vec![Block::Text {
                        text: "effect interrupted by crash; not safely retryable".into(),
                    }],
                    is_error: true,
                }],
            },
        ),
        // Key-deduped effects: re-present the SAME key; the downstream dedupes.
        EffectKind::RequestHumanAction
        | EffectKind::SendPeer
        | EffectKind::ScheduleTimer
        | EffectKind::Compact => Reconciliation::Redispatch { cmd, kind, key },
    }
}

/// The `cmd` a terminal DERIVED result settles, used to detect an outstanding
/// dispatch (a `CommandDispatched` whose `cmd` has no such result in the tail).
/// Covers every `cmd`-correlated result, including the non-fingerprinted abort
/// acks. `ChildReturned` is identity-correlated (no `cmd`) and the EXOGENOUS
/// inputs carry none — both yield `None`.
fn result_cmd(input: &LogicalInput) -> Option<CmdId> {
    match input {
        LogicalInput::ModelResponded { cmd, .. }
        | LogicalInput::ModelFailed { cmd, .. }
        | LogicalInput::InferenceCancelled { cmd, .. }
        | LogicalInput::ToolReturned { cmd, .. }
        | LogicalInput::ToolAborted { cmd, .. }
        | LogicalInput::HumanActionDone { cmd, .. }
        | LogicalInput::HumanActionAborted { cmd, .. }
        | LogicalInput::Compacted { cmd, .. }
        | LogicalInput::PeerSendOutcome { cmd, .. } => Some(*cmd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::event_log::{EventLog, MemoryEventLog};
    use crate::agent::world::history::{Block, History, Msg, Role};
    use crate::agent::world::inputs::{
        Capabilities, Event, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason, Usage,
    };
    use crate::agent::world::model_bridge::to_model_response;
    use crate::agent::world::world::{Effort, ModelConfig, Resources};
    use crate::provider::{ModelApi, ModelInfo, ModelRequest, ModelResponse};
    use std::future::Future;
    use std::pin::Pin;

    /// A model client that fails loudly if invoked. The replay driver takes NO
    /// client (it cannot make a call by construction, Inv 6); this stand-in makes
    /// that guarantee explicit — were any replay path to call a client, the test
    /// would panic here.
    struct ExplodingClient;

    impl ModelApi for ExplodingClient {
        fn turn<'a>(
            &'a self,
            _req: &'a ModelRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
            Box::pin(async { panic!("the replay driver must never invoke the model client") })
        }
        fn list_models<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn model(&self) -> &str {
            "stub"
        }
    }

    /// A `PeerSender` that fails loudly if invoked — the peer-seam analogue of
    /// `ExplodingClient`. The model-path and surface-path tests carry NO `SendPeer`,
    /// so the seam must never be reached; were a drive to route here, this panics.
    struct ExplodingPeerSender;

    impl PeerSender for ExplodingPeerSender {
        async fn send(
            &self,
            _to: &PeerId,
            _payload: &PeerPayload,
            _key: &CommandKey,
            _envelope: &PeerEnvelopeId,
        ) -> Result<DeliveryOutcome, Error> {
            panic!("no SendPeer in this drive; the peer seam must not be reached");
        }
    }

    /// A `PeerSender` returning a SCRIPTED `DeliveryOutcome`, so the SendPeer-arm test
    /// asserts the recorded `PeerSendOutcome` carries exactly what the broker reported.
    /// It also CAPTURES the `(to, envelope)` it was handed so the test can assert the
    /// drive was addressed and the content-addressed envelope minted.
    struct ScriptedPeerSender {
        outcome: DeliveryOutcome,
        seen: std::sync::Mutex<Option<(PeerId, PeerEnvelopeId)>>,
    }

    impl PeerSender for ScriptedPeerSender {
        async fn send(
            &self,
            to: &PeerId,
            _payload: &PeerPayload,
            _key: &CommandKey,
            envelope: &PeerEnvelopeId,
        ) -> Result<DeliveryOutcome, Error> {
            *self.seen.lock().expect("lock seen") = Some((to.clone(), envelope.clone()));
            Ok(self.outcome)
        }
    }

    /// A trivial World whose perceived surface view is empty — the `&World` the
    /// model-path and empty-surface `drive_live` tests pass where they previously
    /// passed `&SurfaceView::new()` (the only thing those drives read from it).
    fn empty_world() -> World {
        World::new(0, Resources::new(7, sample_params()))
    }

    fn sample_params() -> ModelConfig {
        ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        }
    }

    fn sample_call(cmd: CmdId, text: &str) -> Command {
        Command::CallModel {
            cmd,
            entity: 0,
            messages: History(vec![Msg {
                role: Role::User,
                content: vec![Block::Text { text: text.into() }],
            }]),
            tools: ToolSet::default(),
            params: sample_params(),
            key: CommandKey,
        }
    }

    /// Build the `ModelResponded` Event the live driver WOULD have recorded for
    /// `command`, carrying that request's fingerprint — the recorded result the
    /// replay driver stands in.
    fn recorded_response_for(command: &Command, at: Tick) -> Event {
        let Command::CallModel {
            cmd,
            entity,
            messages,
            tools,
            params,
            ..
        } = command
        else {
            panic!("expected CallModel");
        };
        let fingerprint = fingerprint_call(messages, tools, params).expect("fingerprint");
        Event {
            origin: Origin::Agent,
            edge: 0,
            at,
            wall: Some(1_700_000_000),
            input: LogicalInput::ModelResponded {
                cmd: *cmd,
                entity: *entity,
                fingerprint,
                blocks: vec![Block::Text {
                    text: "recorded answer".into(),
                }],
                meta: ModelMeta {
                    usage: Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                    },
                    model_id: "claude-x-2026".into(),
                    stop_reason: StopReason::EndTurn,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        }
    }

    #[test]
    fn replay_reuses_recorded_result_and_never_calls_the_client() {
        let command = sample_call(0, "hello");
        let recorded = recorded_response_for(&command, 2);

        // Pre-load a MemoryEventLog with an EXOGENOUS header (which the cursor must
        // skip) and the recorded result.
        let mut log = MemoryEventLog::new();
        log.append(&Event {
            origin: Origin::System,
            edge: 0,
            at: 0,
            wall: None,
            input: LogicalInput::SessionStarted {
                seed: 7,
                surface_tools: Vec::new(),
            },
        })
        .expect("append header");
        log.append(&recorded).expect("append recorded result");

        let events = log.load().expect("load");
        let mut cursor = ReplayCursor::new(&events);

        // The exploding client is constructed to make the guarantee explicit, but
        // `drive_replay`'s signature takes NO client — it CANNOT be passed, so it
        // cannot be invoked. (If the replay path ever called a client, the panic
        // in `ExplodingClient::call` would fail this test.)
        let _exploding = ExplodingClient;

        let replayed = drive_replay(&[command], &mut cursor).expect("replay");

        assert_eq!(replayed.len(), 1);
        match &replayed[0] {
            Replayed::Reused(event) => {
                assert_eq!(event, &recorded, "replay must feed back the logged Event verbatim");
            }
            other => panic!("expected Reused, got {other:?}"),
        }
    }

    #[test]
    fn replay_diverges_when_the_re_emitted_request_changes() {
        // Record the result for the ORIGINAL prompt.
        let original = sample_call(0, "hello");
        let recorded = recorded_response_for(&original, 2);

        let mut log = MemoryEventLog::new();
        log.append(&recorded).expect("append recorded result");
        let events = log.load().expect("load");
        let mut cursor = ReplayCursor::new(&events);

        // The shell re-emits an EDITED prompt → fingerprint mismatch → the
        // recorded result is stale → fork boundary, NOT a silent stale reuse.
        let edited = sample_call(0, "hello, edited");
        let replayed = drive_replay(&[edited], &mut cursor).expect("replay");

        assert_eq!(replayed, vec![Replayed::Diverged]);
    }

    #[test]
    fn fingerprint_is_deterministic_for_the_same_request() {
        let messages = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text {
                text: "same request".into(),
            }],
        }]);
        let tools = ToolSet::default();
        let params = sample_params();

        // Two independent computations of the SAME request hash identically.
        let a = fingerprint_call(&messages, &tools, &params).expect("fp a");
        let b = fingerprint_call(&messages, &tools, &params).expect("fp b");
        assert_eq!(a, b, "the same request must yield the same Fingerprint");

        // A changed request hashes differently (content-addressed).
        let other = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text {
                text: "a different request".into(),
            }],
        }]);
        let c = fingerprint_call(&other, &tools, &params).expect("fp c");
        assert_ne!(a, c, "a different request must yield a different Fingerprint");
    }

    // --- P2-wal: write-ahead dispatch-intent + ctx inheritance --------------

    /// A model client that returns a fixed successful response, so the live
    /// driver produces a `ModelResponded` without a network.
    struct StubClient;

    impl ModelApi for StubClient {
        fn turn<'a>(
            &'a self,
            _req: &'a ModelRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
            Box::pin(async {
                Ok(to_model_response(
                    vec![Block::Text { text: "ok".into() }],
                    ModelMeta {
                        usage: Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                        },
                        model_id: "claude-x-2026".into(),
                        stop_reason: StopReason::EndTurn,
                        capabilities: Capabilities(serde_json::json!({})),
                        reasoning: ReasoningPolicy::Drop,
                    },
                ))
            })
        }
        fn list_models<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn model(&self) -> &str {
            "stub"
        }
    }

    /// A `SurfaceDriver` that RECORDS each forwarded drive Command, so a test can
    /// assert exactly one surface drive was emitted for a `set_value` `RunTool`
    /// without the macOS app. It is the live-only sink the REPLAY driver never
    /// receives. A `CallModel`-only drive never reaches it, so it doubles as the
    /// no-op sink for the model-path tests.
    #[derive(Default)]
    struct CapturingSurfaceDriver {
        drives: std::sync::Mutex<Vec<Command>>,
    }

    impl SurfaceDriver for CapturingSurfaceDriver {
        async fn drive(&self, command: &Command) -> Result<(), Error> {
            self.drives.lock().expect("lock drives").push(command.clone());
            Ok(())
        }
    }

    /// One append against the log, tagged by stratum, preserving INTERLEAVED
    /// order so a test can assert the write-ahead dispatch-intent (stratum 2)
    /// lands BEFORE the result Event (stratum 1).
    #[derive(Clone)]
    enum Appended {
        Stratum1(Event),
        Stratum2(LifecycleEvent),
    }

    /// An `EventLog` that records the interleaved append order across BOTH
    /// strata, so ordering between a stratum-2 dispatch-intent and a stratum-1
    /// result is observable (the parallel-stream logs cannot show interleaving).
    #[derive(Default)]
    struct OrderedLog {
        appended: Vec<Appended>,
    }

    impl EventLog for OrderedLog {
        fn append(&mut self, event: &Event) -> Result<(), Error> {
            self.appended.push(Appended::Stratum1(event.clone()));
            Ok(())
        }
        fn load(&self) -> Result<Vec<Event>, Error> {
            Ok(self
                .appended
                .iter()
                .filter_map(|a| match a {
                    Appended::Stratum1(e) => Some(e.clone()),
                    Appended::Stratum2(_) => None,
                })
                .collect())
        }
        fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error> {
            self.appended.push(Appended::Stratum2(event.clone()));
            Ok(())
        }
        fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
            Ok(self
                .appended
                .iter()
                .filter_map(|a| match a {
                    Appended::Stratum2(e) => Some(e.clone()),
                    Appended::Stratum1(_) => None,
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn live_driver_write_aheads_dispatch_intent_before_the_result_event() {
        let command = sample_call(3, "hi");
        let stamp = ResultStamp {
            edge: 9,
            app_edge: 1,
            at: 5,
            wall: Some(1_700_000_000),
        };
        let mut log = OrderedLog::default();

        let results = drive_live(
            &[command],
            stamp,
            &empty_world(),
            &StubClient,
            &CapturingSurfaceDriver::default(),
            &ExplodingPeerSender,
            &mut log,
        )
        .await
        .expect("drive");

        // Ordering (Inv 5): the stratum-2 CommandDispatched is appended BEFORE the
        // stratum-1 result Event — intent before commitment.
        assert_eq!(log.appended.len(), 2);
        assert!(
            matches!(
                log.appended[0],
                Appended::Stratum2(LifecycleEvent::CommandDispatched { .. })
            ),
            "the dispatch-intent must be written first"
        );
        assert!(
            matches!(log.appended[1], Appended::Stratum1(_)),
            "the result Event must be written after the dispatch-intent"
        );

        // The dispatch-intent carries ctx, key = (app_id, tick, effect_id), and the
        // request fingerprint.
        let dispatched = log.load_lifecycle().expect("load lifecycle");
        let LifecycleEvent::CommandDispatched {
            cmd,
            at,
            kind,
            ctx,
            key,
            fingerprint,
        } = &dispatched[0]
        else {
            panic!("expected CommandDispatched");
        };
        assert_eq!(*cmd, 3);
        assert_eq!(*at, 5);
        assert_eq!(*kind, EffectKind::CallModel);
        assert_eq!(ctx.entity, 0);
        assert_eq!(ctx.origin, Origin::Agent);
        assert_eq!(ctx.edge, 9);
        assert_eq!(key.tick, 5);
        assert_eq!(key.effect_id, 0);

        // The result Event INHERITS entity/origin/edge from the dispatch-intent's
        // ctx (Inv 17) — not from a positional scan.
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.origin, ctx.origin, "result origin inherited from ctx");
        assert_eq!(result.edge, ctx.edge, "result edge inherited from ctx");
        match &result.input {
            LogicalInput::ModelResponded {
                entity,
                cmd: rcmd,
                fingerprint: rfp,
                ..
            } => {
                assert_eq!(*entity, ctx.entity, "result entity inherited from ctx");
                assert_eq!(*rcmd, 3);
                assert_eq!(rfp, fingerprint, "result fingerprint binds back to the intent");
            }
            other => panic!("expected ModelResponded, got {other:?}"),
        }
    }

    /// W2-tool-exec: driving a `[RunTool set_value]` through the LIVE driver (1)
    /// emits EXACTLY ONE surface drive on the injected sink (the macOS-app forward),
    /// and (2) appends a `ToolReturned` carrying the `{"surface_ops":[...]}`
    /// projection envelope and a UI-request `Fingerprint`. That recorded result
    /// folds via `SurfaceSystem` to a surface-version bump — the agent UI write's
    /// SOLE fact (Inv 18). The model client is NEVER reached (an `ExplodingClient`
    /// proves it), and the `RunTool` dispatch-intent is write-ahead'd in stratum-2.
    #[tokio::test]
    async fn live_driver_executes_set_value_run_tool_emits_drive_and_records_tool_returned() {
        use crate::agent::world::surface::SurfaceSystem;
        use crate::agent::world::systems::System;
        use crate::agent::world::world::{Resources, World};

        // A `set_value` RunTool carrying one surface op in the canonical
        // `args["surface_ops"]` encoding.
        let command = Command::RunTool {
            cmd: 8,
            entity: 0,
            tool: "set_value".into(),
            args: serde_json::json!({
                "surface_ops": [
                    { "op": "set_value", "surface": 9, "element": 2, "value": "typed", "base_version": null }
                ]
            }),
            key: CommandKey,
        };
        let stamp = ResultStamp {
            // A surface-write routes to `app_edge` (3), NOT the conversation `edge` (7).
            edge: 7,
            app_edge: 3,
            at: 4,
            wall: Some(1_700_000_000),
        };
        let mut log = MemoryEventLog::new();
        let sink = CapturingSurfaceDriver::default();

        // The model client must never be invoked for a tool execution: the
        // ExplodingClient panics if reached.
        let results = drive_live(
            &[command],
            stamp,
            &empty_world(),
            &ExplodingClient,
            &sink,
            &ExplodingPeerSender,
            &mut log,
        )
        .await
        .expect("drive_live executes the set_value RunTool");

        // (1) EXACTLY ONE surface drive was forwarded out the sink — the set_value
        // RunTool itself (the production sink converts it to a SurfaceDrive frame).
        {
            let drives = sink.drives.lock().expect("lock drives");
            assert_eq!(drives.len(), 1, "exactly one surface drive emitted for set_value");
            assert!(
                matches!(&drives[0], Command::RunTool { tool, .. } if tool == "set_value"),
                "the forwarded drive is the set_value RunTool"
            );
        }

        // (2) One ToolReturned result Event, inheriting origin/edge from ctx, carrying
        // the projection envelope and the UI-request fingerprint.
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.origin, Origin::Agent, "origin inherited from ctx");
        assert_eq!(
            result.edge, 3,
            "a surface-write ToolReturned routes to the DISTINCT app edge (stamp.app_edge), \
             not the human conversation edge (GAP B)"
        );
        match &result.input {
            LogicalInput::ToolReturned { cmd, entity, .. } => {
                assert_eq!(*cmd, 8, "result correlates back to the dispatched cmd");
                assert_eq!(*entity, 0, "entity inherited from ctx");
            }
            other => panic!("expected ToolReturned, got {other:?}"),
        }

        // The recorded ToolReturned folds via SurfaceSystem to a surface-version bump
        // (0 → 1): the agent write's SOLE record projects the op exactly once (Inv 18),
        // authoring no separate SurfaceMutated and emitting no Commands.
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        };
        let world = World::new(0, Resources::new(7, model));
        let (next, cmds) = SurfaceSystem.step(&world, &result.input);
        assert!(cmds.is_empty(), "a surface projection emits no Commands");
        assert_eq!(
            next.resources.surfaces.get(&9).map(|s| s.version),
            Some(1),
            "the recorded ToolReturned folds via SurfaceSystem to a version bump 0→1"
        );

        // Log-before-apply (Inv 4): the result is durable in stratum-1, and the
        // RunTool dispatch-intent is write-ahead'd in stratum-2 (Inv 5).
        assert_eq!(log.load().expect("load").len(), 1, "one ToolReturned appended");
        assert!(
            matches!(
                log.load_lifecycle().expect("load lifecycle").as_slice(),
                [LifecycleEvent::CommandDispatched {
                    kind: EffectKind::RunTool,
                    ..
                }]
            ),
            "the RunTool dispatch-intent is write-ahead'd in stratum-2"
        );
    }

    /// [W3-tool-decl] The full tool-declaration round-trip: a model `tool_use`
    /// named `set_value` (the declared tool) folds through `turn`/`tool` to a
    /// `Command::RunTool { tool: "set_value", args }` carrying the `tool_use.input`
    /// VERBATIM, and that `args` is accepted by the W2 live executor — driving the
    /// RunTool through `drive_live` forwards EXACTLY ONE SurfaceDrive on the
    /// capturing sink and records ONE `ToolReturned`. This closes the loop W3 opens:
    /// declaring the tool lets a real model request it, and what it requests
    /// executes unchanged. The model client is NEVER reached (ExplodingClient).
    #[tokio::test]
    async fn set_value_tool_use_round_trips_through_turn_tool_and_executes() {
        use crate::agent::world::budget::Budget;
        use crate::agent::world::gates::EntityGate;
        use crate::agent::world::systems::tick;
        use crate::agent::world::world::{
            Activity, Components, Identity, Inbox, Lineage, Resources, World,
        };

        // The tool_use.input the model would produce for the declared set_value
        // schema: the EXACT `{ "surface_ops": [...] }` shape the W2 executor parses.
        let tool_input = serde_json::json!({
            "surface_ops": [
                { "op": "set_value", "surface": 9, "element": 2, "value": "typed", "base_version": null }
            ]
        });

        // An Idle primary entity ready to take a turn.
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        };
        let mut world = World::new(0, Resources::new(7, model));
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History::default(),
                activity: Activity::Idle,
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );

        // A user message opens the turn → CallModel (entity → Thinking on turn_cmd).
        let user = Event {
            origin: Origin::Human,
            edge: 0,
            at: 1,
            wall: None,
            input: LogicalInput::UserMessage {
                to: 0,
                text: "type into the field".into(),
            },
        };
        let (world, commands) = tick(&world, &user);
        let turn_cmd = commands
            .iter()
            .find_map(|c| match c {
                Command::CallModel { cmd, .. } => Some(*cmd),
                _ => None,
            })
            .expect("the turn emits a CallModel");

        // The model responds with a `set_value` tool_use (stop_reason ToolUse):
        // TurnSystem branches into ResolvingToolUses; ToolSystem emits the RunTool.
        let responded = Event {
            origin: Origin::Agent,
            edge: 0,
            at: 2,
            wall: None,
            input: LogicalInput::ModelResponded {
                cmd: turn_cmd,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                blocks: vec![Block::ToolUse {
                    id: "tu_set_value".into(),
                    name: "set_value".into(),
                    input: tool_input.clone(),
                }],
                meta: ModelMeta {
                    usage: Usage::default(),
                    model_id: "claude-x".into(),
                    stop_reason: StopReason::ToolUse,
                    capabilities: Capabilities(serde_json::json!({})),
                    reasoning: ReasoningPolicy::Drop,
                },
            },
        };
        let (_world, commands) = tick(&world, &responded);

        // The set_value tool_use folded to a RunTool carrying the input VERBATIM.
        let run_tool = commands
            .into_iter()
            .find(|c| matches!(c, Command::RunTool { tool, .. } if tool == "set_value"))
            .expect("a set_value tool_use must fold to a RunTool");
        match &run_tool {
            Command::RunTool { tool, args, .. } => {
                assert_eq!(tool, "set_value");
                assert_eq!(
                    args, &tool_input,
                    "the RunTool carries the model tool_use.input unchanged (the W2 executor args)"
                );
            }
            other => panic!("expected RunTool, got {other:?}"),
        }

        // The W2 executor must ACCEPT those args: drive the RunTool through the LIVE
        // driver with a capturing sink → exactly one SurfaceDrive + one ToolReturned.
        let stamp = ResultStamp {
            edge: 0,
            app_edge: 1,
            at: 9,
            wall: None,
        };
        let mut log = MemoryEventLog::new();
        let sink = CapturingSurfaceDriver::default();
        let results = drive_live(
            std::slice::from_ref(&run_tool),
            stamp,
            &empty_world(),
            &ExplodingClient,
            &sink,
            &ExplodingPeerSender,
            &mut log,
        )
        .await
        .expect("the W2 executor accepts the round-tripped set_value args");

        {
            let drives = sink.drives.lock().expect("lock drives");
            assert_eq!(
                drives.len(),
                1,
                "exactly one surface drive forwarded for the round-tripped set_value"
            );
        }
        assert_eq!(results.len(), 1, "one ToolReturned recorded — the agent UI write's sole fact");
        assert!(
            matches!(results[0].input, LogicalInput::ToolReturned { .. }),
            "the executor records a ToolReturned for the accepted args"
        );
    }

    /// [PA-drive-live / VC-3.2] Driving a `Command::SendPeer` through the LIVE driver
    /// (1) routes the drive to the injected `PeerSender` seam (addressed to the decoded
    /// peer, with a content-addressed envelope), and (2) records the seam's returned
    /// `DeliveryOutcome` as a `PeerSendOutcome` whose `entity` is read back from the
    /// `Peer` slot the emitting tick opened — so the PeerDriveSystem settles that slot.
    /// The model client is NEVER reached (`ExplodingClient`), and the `SendPeer`
    /// dispatch-intent is write-ahead'd in stratum-2 (Inv 5).
    #[tokio::test]
    async fn live_driver_dispatches_send_peer_and_records_peer_send_outcome() {
        use crate::agent::world::budget::Budget;
        use crate::agent::world::gates::EntityGate;
        use crate::agent::world::inputs::DriveCommand;
        use crate::agent::world::world::{Components, Identity, Inbox, Lineage, ToolSlot};

        // The owning entity is id 5 (NOT the World root 0), so a correct entity
        // recovery is provably the slot scan, not the root fallback. Its Peer slot is
        // `Pending { cmd: Some(7) }` — the shape PeerDriveSystem leaves after emitting.
        let mut world = empty_world();
        world.entities.insert(
            5,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History::default(),
                activity: Activity::ResolvingToolUses {
                    slots: vec![ToolSlot {
                        tool_use_id: "tu_peer".into(),
                        ordinal: 0,
                        kind: SlotKind::Peer,
                        state: SlotState::Pending { cmd: Some(7) },
                        result: None,
                    }],
                },
                gate: EntityGate::default(),
                budget: Budget::default(),
                inbox: Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );

        let to = PeerId {
            app_id: "app-b".into(),
            node_id: "n1".into(),
        };
        let payload = PeerPayload::Drive(DriveCommand {
            surface_ops: Vec::new(),
            prompt: Some("drive the peer".into()),
        });
        let command = Command::SendPeer {
            cmd: 7,
            to: to.clone(),
            payload: payload.clone(),
            key: CommandKey,
        };
        let stamp = ResultStamp {
            edge: 4,
            app_edge: 1,
            at: 9,
            wall: Some(1_700_000_000),
        };
        let mut log = MemoryEventLog::new();
        let peer_sender = ScriptedPeerSender {
            outcome: DeliveryOutcome::Delivered,
            seen: std::sync::Mutex::new(None),
        };

        let results = drive_live(
            &[command],
            stamp,
            &world,
            &ExplodingClient,
            &CapturingSurfaceDriver::default(),
            &peer_sender,
            &mut log,
        )
        .await
        .expect("drive_live dispatches the SendPeer");

        // (1) The drive was routed to the seam, addressed to the decoded peer, with a
        // content-addressed envelope id derived from the send's fingerprint.
        let expected_fp = fingerprint_peer(&to, &payload).expect("fingerprint");
        let (seen_to, seen_env) = peer_sender
            .seen
            .lock()
            .expect("lock seen")
            .clone()
            .expect("the peer seam was reached exactly once");
        assert_eq!(seen_to, to, "the SendPeer is routed to the decoded peer");
        assert_eq!(
            seen_env,
            PeerEnvelopeId(expected_fp.0.clone()),
            "the envelope is the content-addressed, retry-stable send id"
        );

        // (2) One PeerSendOutcome recorded: entity recovered from the slot (5, not the
        // root 0), correlated by cmd, carrying the seam's outcome and the send fingerprint.
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.origin, Origin::Agent, "origin inherited from ctx");
        assert_eq!(result.edge, 4, "the PeerSendOutcome routes on the turn's edge (stamp.edge)");
        match &result.input {
            LogicalInput::PeerSendOutcome {
                cmd,
                entity,
                fingerprint,
                to: out_to,
                outcome,
            } => {
                assert_eq!(*cmd, 7, "correlates back to the dispatched SendPeer cmd");
                assert_eq!(*entity, 5, "entity recovered from the Peer slot, NOT the root fallback");
                assert_eq!(fingerprint, &expected_fp, "carries the content-addressed send fingerprint");
                assert_eq!(out_to, &to, "carries the peer it was addressed to");
                assert_eq!(*outcome, DeliveryOutcome::Delivered, "records exactly the seam's outcome");
            }
            other => panic!("expected PeerSendOutcome, got {other:?}"),
        }

        // Log-before-apply (Inv 4): the PeerSendOutcome is durable in stratum-1, and
        // the SendPeer dispatch-intent is write-ahead'd in stratum-2 (Inv 5).
        assert_eq!(log.load().expect("load").len(), 1, "one PeerSendOutcome appended");
        assert!(
            matches!(
                log.load_lifecycle().expect("load lifecycle").as_slice(),
                [LifecycleEvent::CommandDispatched {
                    kind: EffectKind::SendPeer,
                    ..
                }]
            ),
            "the SendPeer dispatch-intent is write-ahead'd in stratum-2"
        );
    }

    /// (A) GAP A end-to-end: driving a `CallModel` whose `ToolSet` carries
    /// `set_value` through the LIVE driver assembles a `/v1/messages` request body
    /// whose `tools` array declares the `set_value` schema — so the model is now TOLD
    /// the tool EXISTS and can return a `set_value` tool_use (it provably could not
    /// before, when every CallModel carried an empty ToolSet). A `CallModel` with an
    /// empty `ToolSet` omits the `tools` key entirely (byte-identical to a pre-tools
    /// call). The model client captures the assembled body to assert on it.
    #[tokio::test]
    async fn live_driver_declares_set_value_tool_on_a_surface_capable_call_model() {
        /// A client that CAPTURES the assembled neutral request, then returns a stub
        /// reply — so a test can assert the request declares the offered tools.
        #[derive(Default)]
        struct BodyCapturingClient {
            request: std::sync::Mutex<Option<ModelRequest>>,
        }
        impl ModelApi for BodyCapturingClient {
            fn turn<'a>(
                &'a self,
                req: &'a ModelRequest,
            ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
                *self.request.lock().expect("lock request") = Some(req.clone());
                Box::pin(async {
                    Ok(to_model_response(
                        vec![Block::Text { text: "ok".into() }],
                        ModelMeta {
                            usage: Usage {
                                input_tokens: 1,
                                output_tokens: 1,
                            },
                            model_id: "claude-x-2026".into(),
                            stop_reason: StopReason::EndTurn,
                            capabilities: Capabilities(serde_json::json!({})),
                            reasoning: ReasoningPolicy::Drop,
                        },
                    ))
                })
            }
            fn list_models<'a>(
                &'a self,
            ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn model(&self) -> &str {
                "stub"
            }
        }

        let stamp = ResultStamp {
            edge: 0,
            app_edge: 1,
            at: 1,
            wall: None,
        };

        // A surface-capable CallModel: its ToolSet carries `set_value`.
        let surface_call = Command::CallModel {
            cmd: 1,
            entity: 0,
            messages: History(vec![Msg {
                role: Role::User,
                content: vec![Block::Text {
                    text: "type into the field".into(),
                }],
            }]),
            tools: ToolSet(vec!["set_value".into()]),
            params: sample_params(),
            key: CommandKey,
        };
        let client = BodyCapturingClient::default();
        let mut log = MemoryEventLog::new();
        drive_live(
            &[surface_call],
            stamp,
            &empty_world(),
            &client,
            &CapturingSurfaceDriver::default(),
            &ExplodingPeerSender,
            &mut log,
        )
        .await
        .expect("drive the surface-capable CallModel");

        let request = client
            .request
            .lock()
            .expect("lock")
            .clone()
            .expect("a request was assembled");
        assert_eq!(request.tools.len(), 1, "exactly the one declared surface tool");
        assert_eq!(request.tools[0].name, "set_value", "the model is told `set_value` EXISTS");
        assert_eq!(
            request.tools[0].input_schema["type"], "object",
            "the declaration carries the set_value input_schema"
        );

        // A tool-less CallModel (empty ToolSet) declares no tools — byte-identical
        // to a pre-tools call (replay/fingerprint stability).
        let plain_call = Command::CallModel {
            cmd: 2,
            entity: 0,
            messages: History(vec![Msg {
                role: Role::User,
                content: vec![Block::Text { text: "hi".into() }],
            }]),
            tools: ToolSet::default(),
            params: sample_params(),
            key: CommandKey,
        };
        let plain_client = BodyCapturingClient::default();
        let mut plain_log = MemoryEventLog::new();
        drive_live(
            &[plain_call],
            stamp,
            &empty_world(),
            &plain_client,
            &CapturingSurfaceDriver::default(),
            &ExplodingPeerSender,
            &mut plain_log,
        )
        .await
        .expect("drive the tool-less CallModel");
        let plain_request = plain_client
            .request
            .lock()
            .expect("lock")
            .clone()
            .expect("request");
        assert!(
            plain_request.tools.is_empty(),
            "an empty ToolSet declares no tools (byte-identical to a pre-tools call)"
        );
    }

    #[test]
    fn replay_fold_ignores_stratum_two_records() {
        // The recorded result for an original prompt.
        let command = sample_call(0, "hello");
        let recorded = recorded_response_for(&command, 2);
        let LogicalInput::ModelResponded { fingerprint, .. } = &recorded.input else {
            panic!("expected ModelResponded fixture");
        };
        let dispatch = LifecycleEvent::CommandDispatched {
            at: 2,
            cmd: 0,
            kind: EffectKind::CallModel,
            ctx: ActorCtx {
                entity: 0,
                origin: Origin::Agent,
                edge: 0,
            },
            key: IdempotencyKey {
                app_id: AppId::default(),
                tick: 2,
                effect_id: 0,
            },
            fingerprint: fingerprint.clone(),
        };

        // Log A: the stratum-1 result only.
        let mut bare = MemoryEventLog::new();
        bare.append(&recorded).expect("append result");

        // Log B: the SAME stratum-1 result, PLUS stratum-2 dispatch-intents around it.
        let mut with_wal = MemoryEventLog::new();
        with_wal.append_lifecycle(&dispatch).expect("append intent");
        with_wal.append(&recorded).expect("append result");
        with_wal.append_lifecycle(&dispatch).expect("append intent");

        // The replay path consumes `load()` (stratum 1 only), so the two logs
        // present an IDENTICAL event sequence — stratum 2 is neutral on replay.
        let events_bare = bare.load().expect("load bare");
        let events_wal = with_wal.load().expect("load wal");
        assert_eq!(
            events_bare, events_wal,
            "stratum-2 records must not reach the replay fold"
        );

        // And the replay driver yields IDENTICAL outcomes with or without them
        // present — a World folded from either log is identical.
        let mut c_bare = ReplayCursor::new(&events_bare);
        let mut c_wal = ReplayCursor::new(&events_wal);
        let r_bare = drive_replay(&[command.clone()], &mut c_bare).expect("replay bare");
        let r_wal = drive_replay(&[command], &mut c_wal).expect("replay wal");
        assert_eq!(
            r_bare, r_wal,
            "replay outcome must be identical with or without stratum 2 present"
        );
    }

    // --- P1a Command round-trip tests ---------------------------------------
    //
    // `Command` deliberately does NOT derive `Serialize`/`Deserialize` (it owns
    // `History` and `ModelConfig` which are not worth double-serialising just for
    // these tests), so we verify round-trips via `Debug` equality and structural
    // inspection of the variant fields instead.

    #[test]
    fn cancel_inference_is_constructible() {
        let cmd = Command::CancelInference { cmd: 7 };
        // Exhaustive destructure — proves no extra fields exist.
        let Command::CancelInference { cmd: c } = cmd else {
            panic!("unexpected variant");
        };
        assert_eq!(c, 7);
    }

    #[test]
    fn run_tool_is_constructible() {
        let cmd = Command::RunTool {
            cmd: 8,
            entity: 0,
            tool: "get_weather".into(),
            args: serde_json::json!({ "city": "Tokyo" }),
            key: CommandKey,
        };
        let Command::RunTool { cmd: c, entity: e, tool: t, args: a, key: _ } = cmd else {
            panic!("unexpected variant");
        };
        assert_eq!(c, 8);
        assert_eq!(e, 0);
        assert_eq!(t, "get_weather");
        assert_eq!(a, serde_json::json!({ "city": "Tokyo" }));
    }

    #[test]
    fn cancel_tool_is_constructible() {
        let cmd = Command::CancelTool { cmd: 9 };
        let Command::CancelTool { cmd: c } = cmd else {
            panic!("unexpected variant");
        };
        assert_eq!(c, 9);
    }

    #[test]
    fn request_human_action_is_constructible() {
        use crate::agent::world::autonomy::{HumanAction, Notify};
        let cmd = Command::RequestHumanAction {
            cmd: 10,
            entity: 1,
            ask: HumanAction::Prompt { text: "Please confirm.".into() },
            notify: Notify::Push,
            key: CommandKey,
        };
        let Command::RequestHumanAction { cmd: c, entity: e, ask: _, notify: n, key: _ } = cmd
        else {
            panic!("unexpected variant");
        };
        assert_eq!(c, 10);
        assert_eq!(e, 1);
        assert_eq!(n, Notify::Push);
    }

    #[test]
    fn abort_human_action_is_constructible() {
        let cmd = Command::AbortHumanAction { cmd: 11 };
        let Command::AbortHumanAction { cmd: c } = cmd else {
            panic!("unexpected variant");
        };
        assert_eq!(c, 11);
    }

    #[test]
    fn raise_interaction_is_constructible() {
        use crate::agent::world::autonomy::AgentInteraction;
        let cmd = Command::RaiseInteraction {
            request_id: 5,
            entity: 2,
            interaction: AgentInteraction {
                payload: serde_json::json!({ "kind": "approval" }),
            },
        };
        let Command::RaiseInteraction { request_id: r, entity: e, interaction: _ } = cmd else {
            panic!("unexpected variant");
        };
        assert_eq!(r, 5);
        assert_eq!(e, 2);
    }

    #[test]
    fn compact_is_constructible() {
        let messages = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text { text: "old message".into() }],
        }]);
        let params = sample_params();
        let cmd = Command::Compact {
            cmd: 20,
            entity: 3,
            upto: 1,
            messages: messages.clone(),
            params: params.clone(),
            key: CommandKey,
        };
        let Command::Compact {
            cmd: c,
            entity: e,
            upto: u,
            messages: m,
            params: p,
            key: _,
        } = cmd
        else {
            panic!("unexpected variant");
        };
        assert_eq!(c, 20);
        assert_eq!(e, 3);
        assert_eq!(u, 1);
        assert_eq!(m, messages);
        assert_eq!(p, params);
    }

    #[test]
    fn fingerprint_compact_is_deterministic_for_the_same_request() {
        let messages = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text { text: "old msg".into() }],
        }]);
        let params = sample_params();

        let a = fingerprint_compact(&messages, &params).expect("fp a");
        let b = fingerprint_compact(&messages, &params).expect("fp b");
        assert_eq!(a, b, "the same compact request must yield the same Fingerprint");

        let other = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Text { text: "different msg".into() }],
        }]);
        let c = fingerprint_compact(&other, &params).expect("fp c");
        assert_ne!(a, c, "a different compact request must yield a different Fingerprint");
    }

    // --- P2-crash: resume reconciliation (replay → resume edge) --------------

    /// A dispatch-intent fixture: the write-ahead `CommandDispatched` the live
    /// driver appends BEFORE an effect, carrying the durable `cmd → ctx` index,
    /// the idempotency `key`, and the request `fingerprint`.
    fn dispatch(cmd: CmdId, kind: EffectKind, entity: EntityId, edge: EdgeId) -> LifecycleEvent {
        LifecycleEvent::CommandDispatched {
            at: 3,
            cmd,
            kind,
            ctx: ActorCtx {
                entity,
                origin: Origin::Agent,
                edge,
            },
            key: IdempotencyKey {
                app_id: AppId::default(),
                tick: 3,
                effect_id: 0,
            },
            fingerprint: Fingerprint(format!("fp-{cmd}")),
        }
    }

    /// A minimal stratum-1 tail: a session header plus a user message — NO result
    /// for the dispatched `cmd` (truncated mid-effect by the crash).
    fn truncated_tail() -> Vec<Event> {
        vec![
            Event {
                origin: Origin::System,
                edge: 0,
                at: 0,
                wall: Some(1_700_000_000),
                input: LogicalInput::SessionStarted {
                    seed: 7,
                    surface_tools: Vec::new(),
                },
            },
            Event {
                origin: Origin::Human,
                edge: 9,
                at: 1,
                wall: None,
                input: LogicalInput::UserMessage {
                    to: 0,
                    text: "hello".into(),
                },
            },
        ]
    }

    /// VC-2.2: a `CommandDispatched { CallModel }` with NO matching `ModelResponded`
    /// in the tail (crash mid-`Thinking`) → resume synthesises AND LOGS a stratum-1
    /// `InferenceCancelled { Crash }` carrying the dangling dispatch's
    /// `cmd`/`entity`/`fingerprint`, with the Event envelope inheriting
    /// `origin`/`edge` from the dispatch `ctx` (Inv 17). (Inv 5, 6, 10.)
    #[test]
    fn resume_synthesises_and_logs_inference_cancelled_for_a_thinking_crash() {
        let mut log = MemoryEventLog::new();
        for e in truncated_tail() {
            log.append(&e).expect("append tail");
        }
        let outstanding = dispatch(5, EffectKind::CallModel, 0, 9);
        log.append_lifecycle(&outstanding).expect("append intent");

        let events = log.load().expect("load stratum-1");
        let lifecycle = log.load_lifecycle().expect("load stratum-2");
        let reconciled = resume(&events, &lifecycle, &mut log).expect("resume");

        // Exactly one reconciliation: the outstanding CallModel settles.
        assert_eq!(reconciled.len(), 1);
        let settled = match &reconciled[0] {
            Reconciliation::Settled(event) => event,
            other => panic!("expected Settled, got {other:?}"),
        };
        // The Event envelope inherits origin/edge from the dispatch ctx (Inv 17).
        assert_eq!(settled.origin, Origin::Agent, "origin inherited from ctx");
        assert_eq!(settled.edge, 9, "edge inherited from ctx");
        // The synthesised result fills cmd/entity/fingerprint from the dangling
        // CommandDispatched and is a Crash cancellation with no partial.
        match &settled.input {
            LogicalInput::InferenceCancelled {
                cmd,
                entity,
                fingerprint,
                partial,
                reason,
            } => {
                assert_eq!(*cmd, 5, "cmd from the dangling dispatch");
                assert_eq!(*entity, 0, "entity from the dispatch ctx");
                assert_eq!(*fingerprint, Fingerprint("fp-5".into()), "fingerprint bound back");
                assert_eq!(*partial, None);
                assert_eq!(*reason, CancelReason::Crash);
            }
            other => panic!("expected InferenceCancelled, got {other:?}"),
        }

        // The crash is a REAL logged stratum-1 Input (not a silent rewind): it is
        // appended to the tail and binds the same cmd.
        let after = log.load().expect("reload stratum-1");
        assert_eq!(after.len(), 3, "the synthesised cancellation was appended");
        assert!(
            matches!(
                &after[2].input,
                LogicalInput::InferenceCancelled { cmd: 5, reason: CancelReason::Crash, .. }
            ),
            "the synthesised InferenceCancelled is durable in the tail"
        );

        // Feeding the synthesised Input to the TurnSystem settles Thinking → Idle.
        use crate::agent::world::gates::EntityGate;
        use crate::agent::world::systems::System;
        use crate::agent::world::systems::turn::TurnSystem;
        use crate::agent::world::world::{
            Activity, Components, Identity, Lineage, Resources, World,
        };
        let model = ModelConfig {
            model: "claude-x".into(),
            max_tokens: 1024,
            effort: Effort::Medium,
        };
        let mut world = World::new(0, Resources::new(7, model));
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History::default(),
                activity: Activity::Thinking { cmd: 5 },
                gate: EntityGate::default(),
                budget: crate::agent::world::budget::Budget::default(),
                inbox: crate::agent::world::world::Inbox::default(),
                turns: 0,
                spawned: 0,
                model: None,
                autonomy: None,
            },
        );
        let (settled_world, commands) = TurnSystem.step(&world, &settled.input);
        assert!(commands.is_empty(), "a crash cancellation emits no continuation here");
        assert!(
            matches!(
                settled_world.entities.get(&0).expect("entity").activity,
                Activity::Idle
            ),
            "InferenceCancelled{{Crash}} settles Thinking → Idle (totality, Inv 10)"
        );
    }

    /// VC-2.2: a non-idempotent effectful tool dispatch with no result in the tail
    /// resolves with an `is_error` `ToolResult` (surfaced-as-failed), filling
    /// `cmd`/`entity`/`fingerprint` from the dangling dispatch.
    #[test]
    fn resume_surfaces_a_non_idempotent_tool_as_is_error() {
        let mut log = MemoryEventLog::new();
        for e in truncated_tail() {
            log.append(&e).expect("append tail");
        }
        log.append_lifecycle(&dispatch(8, EffectKind::RunTool, 0, 9))
            .expect("append intent");

        let events = log.load().expect("load stratum-1");
        let lifecycle = log.load_lifecycle().expect("load stratum-2");
        let reconciled = resume(&events, &lifecycle, &mut log).expect("resume");

        assert_eq!(reconciled.len(), 1);
        let settled = match &reconciled[0] {
            Reconciliation::Settled(event) => event,
            other => panic!("expected Settled, got {other:?}"),
        };
        match &settled.input {
            LogicalInput::ToolReturned {
                cmd,
                entity,
                fingerprint,
                result,
            } => {
                assert_eq!(*cmd, 8, "cmd from the dangling dispatch");
                assert_eq!(*entity, 0, "entity from the dispatch ctx");
                assert_eq!(*fingerprint, Fingerprint("fp-8".into()));
                // The crash result rides as an is_error ToolResult.
                assert!(
                    matches!(
                        result.as_slice(),
                        [Block::ToolResult { is_error: true, .. }]
                    ),
                    "a non-idempotent tool crash surfaces-as-failed (is_error)"
                );
            }
            other => panic!("expected ToolReturned, got {other:?}"),
        }
    }

    /// A key-deduped effect (`Compact`/`RequestHumanAction`/`SendPeer`/
    /// `ScheduleTimer`) re-dispatches with the SAME `key` rather than settling.
    #[test]
    fn resume_redispatches_key_deduped_effects_with_the_same_key() {
        let mut log = MemoryEventLog::new();
        for e in truncated_tail() {
            log.append(&e).expect("append tail");
        }
        let outstanding = dispatch(12, EffectKind::Compact, 0, 9);
        log.append_lifecycle(&outstanding).expect("append intent");

        let events = log.load().expect("load stratum-1");
        let lifecycle = log.load_lifecycle().expect("load stratum-2");
        let reconciled = resume(&events, &lifecycle, &mut log).expect("resume");

        assert_eq!(reconciled.len(), 1);
        match &reconciled[0] {
            Reconciliation::Redispatch { cmd, kind, key } => {
                assert_eq!(*cmd, 12);
                assert_eq!(*kind, EffectKind::Compact);
                // The SAME key is re-presented (downstream dedupes).
                assert_eq!(key.tick, 3);
                assert_eq!(key.effect_id, 0);
            }
            other => panic!("expected Redispatch, got {other:?}"),
        }
        // A re-dispatch logs NO stratum-1 result (the live driver re-emits it).
        assert_eq!(log.load().expect("reload").len(), 2, "no result was synthesised");
    }

    /// A dispatch whose `cmd` already has a terminal result in the tail committed
    /// before the crash — resume skips it (no double settlement).
    #[test]
    fn resume_skips_a_dispatch_that_already_has_a_result() {
        let command = sample_call(7, "hello");
        let recorded = recorded_response_for(&command, 2);

        let mut log = MemoryEventLog::new();
        for e in truncated_tail() {
            log.append(&e).expect("append tail");
        }
        log.append(&recorded).expect("append result");
        // The dispatch-intent for the SAME cmd that already resolved.
        log.append_lifecycle(&dispatch(7, EffectKind::CallModel, 0, 9))
            .expect("append intent");

        let events = log.load().expect("load stratum-1");
        let lifecycle = log.load_lifecycle().expect("load stratum-2");
        let reconciled = resume(&events, &lifecycle, &mut log).expect("resume");

        assert!(
            reconciled.is_empty(),
            "a dispatch with a logged result is settled — nothing to reconcile"
        );
    }

    // -----------------------------------------------------------------------
    // BL-cap: inline-vs-externalize policy + shell put/get (VC-1.1 / VC-1.2)
    // -----------------------------------------------------------------------

    /// The PURE externalize decision is a function of `(len, cap)` only — no IO:
    /// at/under the cap stays inline, strictly over externalizes, and `cap == 0`
    /// (unbounded) never externalizes however large the payload.
    #[test]
    fn over_inline_cap_is_a_pure_decision_of_len_and_cap() {
        // cap == 0 ⇒ unbounded: never externalize, regardless of size.
        assert!(!over_inline_cap(0, 0));
        assert!(!over_inline_cap(1_000_000, 0));
        // At/under the cap ⇒ inline.
        assert!(!over_inline_cap(8, 8), "exactly at the cap stays inline");
        assert!(!over_inline_cap(7, 8), "under the cap stays inline");
        // Strictly over the cap ⇒ externalize.
        assert!(over_inline_cap(9, 8), "over the cap externalizes");
    }

    /// A model client returning a SCRIPTED set of blocks and capturing the neutral
    /// request it was handed — so a test asserts both the externalize of the RESPONSE
    /// and the resolve of the REQUEST against a real `BlobStore`. Its canned reply is
    /// expressed in world `Block`s and converted at the seam, so the test's intent is
    /// unchanged.
    struct ScriptedBlocksClient {
        blocks: Vec<Block>,
        request: std::sync::Mutex<Option<ModelRequest>>,
    }

    impl ModelApi for ScriptedBlocksClient {
        fn turn<'a>(
            &'a self,
            req: &'a ModelRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
            *self.request.lock().expect("lock request") = Some(req.clone());
            let blocks = self.blocks.clone();
            Box::pin(async move {
                Ok(to_model_response(
                    blocks,
                    ModelMeta {
                        usage: Usage {
                            input_tokens: 1,
                            output_tokens: 1,
                        },
                        model_id: "claude-x-2026".into(),
                        stop_reason: StopReason::EndTurn,
                        capabilities: Capabilities(serde_json::json!({})),
                        reasoning: ReasoningPolicy::Drop,
                    },
                ))
            })
        }
        fn list_models<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn model(&self) -> &str {
            "stub"
        }
    }

    fn world_with_cap(cap: ByteCap) -> World {
        let mut world = empty_world();
        world.resources.caps.blob_inline_cap = cap;
        world
    }

    fn call_with_history(cmd: CmdId, messages: History) -> Command {
        Command::CallModel {
            cmd,
            entity: 0,
            messages,
            tools: ToolSet::default(),
            params: sample_params(),
            key: CommandKey,
        }
    }

    fn no_blob_stamp() -> ResultStamp {
        ResultStamp {
            edge: 0,
            app_edge: 1,
            at: 1,
            wall: None,
        }
    }

    /// VC-1.1 (shell round-trip): a model response carrying an over-cap image is
    /// externalized via `BlobStore.put` and recorded as `ImageSource::Blob{hash}` —
    /// the recorded `ModelResponded` holds the HASH, never the bytes — and the blob
    /// round-trips by hash through the store (`get(hash) == original bytes`).
    #[tokio::test]
    async fn over_cap_image_is_externalized_to_blob_and_round_trips_by_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let big = vec![0xABu8; 64]; // 64 bytes — over a cap of 8.
        let client = ScriptedBlocksClient {
            blocks: vec![
                Block::Text {
                    text: "see image".into(),
                },
                Block::Image {
                    source: ImageSource::Inline {
                        mime: "image/png".into(),
                        bytes: big.clone(),
                    },
                },
            ],
            request: std::sync::Mutex::new(None),
        };
        let mut log = MemoryEventLog::new();

        let results = drive_live_with_blob_sink(
            &[call_with_history(1, History::default())],
            no_blob_stamp(),
            &world_with_cap(8),
            &client,
            &CapturingSurfaceDriver::default(),
            &ExplodingPeerSender,
            &store,
            &mut log,
        )
        .await
        .expect("drive externalizes the over-cap image");

        let LogicalInput::ModelResponded { blocks, .. } = &results[0].input else {
            panic!("expected ModelResponded");
        };
        // The image block now carries only the content-address hash, not the bytes.
        let hash = match &blocks[1] {
            Block::Image {
                source: ImageSource::Blob { hash, mime },
            } => {
                assert_eq!(mime, "image/png", "the mime is preserved across externalize");
                hash.clone()
            }
            other => panic!("expected an externalized Blob image, got {other:?}"),
        };
        // The recorded Event holds the hash as a blob reference — no inline bytes.
        let recorded = serde_json::to_string(&results[0]).expect("serialize event");
        assert!(recorded.contains(&hash.0), "the log carries the blob hash");
        assert!(
            recorded.contains("\"type\":\"blob\""),
            "the image is recorded as a blob reference"
        );
        assert!(
            !recorded.contains("\"type\":\"inline\""),
            "no inline bytes are recorded in the log (only the hash)"
        );

        // The blob round-trips by hash through the store.
        assert_eq!(
            store.get(&hash).expect("get blob"),
            big,
            "put → Blob{{hash}} → get returns the original bytes"
        );
    }

    /// VC-1.2: a payload at/under the cap stays `ImageSource::Inline`, and `cap == 0`
    /// (unbounded) never externalizes however large the payload — the store stays
    /// empty in both cases (no `put`).
    #[tokio::test]
    async fn under_cap_and_zero_cap_keep_image_inline() {
        for (cap, bytes) in [(64u32, vec![1u8; 8]), (0u32, vec![1u8; 4096])] {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = BlobStore::new(dir.path().join("blobs"));
            let client = ScriptedBlocksClient {
                blocks: vec![Block::Image {
                    source: ImageSource::Inline {
                        mime: "image/png".into(),
                        bytes: bytes.clone(),
                    },
                }],
                request: std::sync::Mutex::new(None),
            };
            let mut log = MemoryEventLog::new();
            let results = drive_live_with_blob_sink(
                &[call_with_history(1, History::default())],
                no_blob_stamp(),
                &world_with_cap(cap),
                &client,
                &CapturingSurfaceDriver::default(),
                &ExplodingPeerSender,
                &store,
                &mut log,
            )
            .await
            .expect("drive keeps the image inline");

            let LogicalInput::ModelResponded { blocks, .. } = &results[0].input else {
                panic!("expected ModelResponded");
            };
            assert!(
                matches!(
                    &blocks[0],
                    Block::Image { source: ImageSource::Inline { bytes: b, .. } } if *b == bytes
                ),
                "an at/under-cap (or cap==0) image stays Inline with its bytes (cap={cap})"
            );
            assert!(
                store.stored_hashes().expect("stored").is_empty(),
                "no put for an inline image (cap={cap})"
            );
        }
    }

    /// VC-1.1 (read path): a request built from a History carrying a `Blob{hash}`
    /// resolves the bytes through `BlobStore.get`, so the model receives the real
    /// bytes — while the request FINGERPRINT stays over the Blob-form History, so a
    /// replay (which fingerprints over the recorded Blob-form History) hashes
    /// identically (Inv 7).
    #[tokio::test]
    async fn request_body_resolves_a_blob_hash_back_to_inline_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));
        let bytes = vec![9u8, 8, 7, 6, 5];
        let hash = store.put(&bytes).expect("seed blob");

        // A prior turn externalized this image; History now carries the Blob ref.
        let messages = History(vec![Msg {
            role: Role::User,
            content: vec![Block::Image {
                source: ImageSource::Blob {
                    hash: hash.clone(),
                    mime: "image/png".into(),
                },
            }],
        }]);

        // The fingerprint the replay driver recomputes is over the Blob-form messages.
        let blob_form_fp = fingerprint_call(&messages, &ToolSet::default(), &sample_params())
            .expect("fingerprint over blob-form messages");

        let client = ScriptedBlocksClient {
            blocks: vec![Block::Text { text: "ok".into() }],
            request: std::sync::Mutex::new(None),
        };
        let mut log = MemoryEventLog::new();
        let results = drive_live_with_blob_sink(
            &[call_with_history(1, messages.clone())],
            no_blob_stamp(),
            &world_with_cap(4),
            &client,
            &CapturingSurfaceDriver::default(),
            &ExplodingPeerSender,
            &store,
            &mut log,
        )
        .await
        .expect("drive resolves the blob for the body");

        // The neutral request the model received carries the RESOLVED inline bytes,
        // not the hash: the world `Blob{hash}` was resolved to inline bytes and the
        // seam mapped that to a neutral `Base64` image source carrying the bytes.
        use base64::Engine;
        let request = client.request.lock().expect("lock").clone().expect("request");
        let image = &request.messages[0].content[0];
        match image {
            crate::provider::ContentBlock::Image {
                source: crate::provider::ImageSource::Base64 { media_type, data },
            } => {
                assert_eq!(media_type, "image/png", "the resolved image carries its mime");
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data.as_bytes())
                    .expect("the resolved base64 decodes");
                assert_eq!(
                    decoded.len(),
                    bytes.len(),
                    "the Blob{{hash}} was resolved to the original inline bytes for the model"
                );
            }
            other => panic!("expected a resolved inline (Base64) image, got {other:?}"),
        }

        // The recorded result's fingerprint binds to the Blob-form request, so a
        // replay (fingerprinting over the recorded Blob-form History) matches.
        let LogicalInput::ModelResponded { fingerprint, .. } = &results[0].input else {
            panic!("expected ModelResponded");
        };
        assert_eq!(
            fingerprint, &blob_form_fp,
            "the request fingerprint stays over the Blob-form History (replay-stable)"
        );
    }

    /// Task-local byte-identity: a no-image response with a cap set externalizes
    /// NOTHING (no Image block), so the recorded `ModelResponded` carries exactly the
    /// model's blocks and the store stays empty — the no-image path is untouched.
    #[tokio::test]
    async fn no_image_response_is_byte_identical_and_touches_no_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));
        let client = ScriptedBlocksClient {
            blocks: vec![Block::Text {
                text: "plain reply".into(),
            }],
            request: std::sync::Mutex::new(None),
        };
        let mut log = MemoryEventLog::new();
        let results = drive_live_with_blob_sink(
            &[call_with_history(
                1,
                History(vec![Msg {
                    role: Role::User,
                    content: vec![Block::Text { text: "hi".into() }],
                }]),
            )],
            no_blob_stamp(),
            &world_with_cap(8),
            &client,
            &CapturingSurfaceDriver::default(),
            &ExplodingPeerSender,
            &store,
            &mut log,
        )
        .await
        .expect("drive a no-image turn");

        let LogicalInput::ModelResponded { blocks, .. } = &results[0].input else {
            panic!("expected ModelResponded");
        };
        assert_eq!(
            blocks.as_slice(),
            [Block::Text {
                text: "plain reply".into()
            }],
            "no-image blocks are recorded verbatim (byte-identity)"
        );
        assert!(
            store.stored_hashes().expect("stored").is_empty(),
            "a no-image turn writes no blob"
        );
    }
}
