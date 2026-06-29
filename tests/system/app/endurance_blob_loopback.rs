//! **VC-1.4** — the blob externalize+retrieve LIVE loopback: a turn carrying a REAL
//! large payload (an image over `Caps.blob_inline_cap`) externalizes it to the
//! `BlobStore` and retrieves it BY HASH, proven against a REAL model call, NO VM, no
//! surface, no subprocess worker.
//!
//! The offline half (`tests/integration/agent/world_blob_store.rs`, VC-1.1/1.3)
//! proves — deterministically, non-vacuously — the externalize round-trip by hash,
//! the byte-identical replay of an externalized-blob log, and the reachability GC
//! keep/reclaim, all against the REAL `BlobStore` but with no model call. This case
//! proves the LIVE side the offline half cannot: that a turn driven through the
//! PRODUCTION live driver ([`drive_live_with_blob_sink`]) against a REAL provider
//! externalizes a real over-cap payload to a real `BlobStore` and that the bytes
//! come back BY HASH through `BlobStore::get`. Per the mock-data policy (root
//! `CLAUDE.md`) it uses the real model and is gated on live-LLM credentials,
//! skipping cleanly when absent.
//!
//! ## The construction and the proof
//! The World runs with a positive `Caps.blob_inline_cap` (1 KiB), so any payload
//! over it MUST externalize. A single primary entity (model = the real env model)
//! is driven through one turn:
//!
//! 1. The user message drives the pure `tick` reducer, which emits the turn's
//!    `CallModel`.
//! 2. That `CallModel` is dispatched through the production
//!    [`drive_live_with_blob_sink`] against the REAL selected provider, wrapped so the
//!    turn's response carries a REAL large image payload (over the cap) alongside the
//!    model's genuine text — exactly the shape a multimodal turn (or a large
//!    image-bearing tool result folded into the response) produces. The model CALL
//!    is real (text request → real text answer); the large image is the real payload
//!    the turn carries and the production externalize must store.
//! 3. The driver's externalize policy (`> blob_inline_cap`) `put`s the image to the
//!    real `BlobStore` and records the result `ModelResponded` carrying only
//!    `ImageSource::Blob{hash}` — the log holds the HASH, never the bytes.
//!
//! Two independent facts pin the externalize+retrieve:
//!
//! 1. **The log holds the hash, not the bytes.** The recorded `ModelResponded`
//!    carries `ImageSource::Blob{hash}` and serializes with `"type":"blob"` and the
//!    content address — never `"type":"inline"` for the over-cap image. The call
//!    really happened against the real model (a `ModelResponded`, not `ModelFailed`).
//! 2. **The bytes come from `BlobStore::get`.** `store.get(hash)` returns the EXACT
//!    real payload bytes — the externalized blob round-trips by hash through the real
//!    durable store.
//!
//! The dispatched result Events are persisted under
//! `tests/results/.../system/endurance_blob_loopback/` for debugging (per
//! `tests/CLAUDE.md`).
//!
//! See `docs/agent/world/ecs-runtime.md` §1415-1434 (content-addressed durable blob
//! store = PRIMARY storage; externalize over the inline cap; resolve/get by hash)
//! and the plan's Verification §1 VC-1.4.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;

use rubberdux::agent::world::blob::BlobStore;
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{
    drive_live_with_blob_sink, Command, ResultStamp, SurfaceDriver,
    UnattachedPeerSender,
};
use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{Block, History, ImageSource};
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};
use rubberdux::agent::world::world::{
    Activity, Components, Effort, EntityId, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;
use rubberdux::provider::{
    ContentBlock, ImageSource as NeutralImageSource, ModelApi, ModelInfo, ModelRequest,
    ModelResponse, selected_from_env,
};
use std::future::Future;
use std::pin::Pin;

use crate::live_gate::skip_without_live_llm;

// The hidden RNG seed crossing the recorded boundary (Inv 8).
const SEED: u64 = 41;

// The primary (lead) entity whose turn carries the large payload.
const PRIMARY: EntityId = 0;

// The inline cap: any payload over 1 KiB externalizes. The real image below is far
// larger, so it MUST be stored to the BlobStore (not inlined).
const BLOB_INLINE_CAP: u32 = 1024;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The genesis World: a single primary entity (model = the real env model) running
/// with a positive `blob_inline_cap`, so the turn's over-cap image externalizes.
fn genesis(model: &ModelConfig) -> World {
    let mut world = World::new(PRIMARY, Resources::new(SEED, model.clone()));
    // Engage the externalize policy: a payload over this cap is stored to the blob
    // store and recorded as `ImageSource::Blob{hash}`.
    world.resources.caps.blob_inline_cap = BLOB_INLINE_CAP;
    world.entities.insert(
        PRIMARY,
        Components {
            identity: Identity::Primary,
            lineage: Lineage {
                parent: None,
                depth: 0,
            },
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
    world
}

/// The world-default `ModelConfig` (the real env model the live call targets).
/// `model` is the alias `selected_from_env` resolved from `RUBBERDUX_LLM_MODEL`
/// (so a REAL call hits a valid model); `max_tokens` from `RUBBERDUX_LLM_MAX_TOKENS`
/// (default 1024); effort `Medium`. Mirrors `endurance_overrides_loopback`.
fn env_model_config(model_alias: &str) -> ModelConfig {
    let max_tokens = std::env::var("RUBBERDUX_LLM_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1024);
    ModelConfig {
        model: model_alias.to_string(),
        max_tokens,
        effort: Effort::Medium,
    }
}

/// A REAL large image payload (64 KiB, far over the 1 KiB cap) the turn carries. It
/// opens with the PNG magic bytes and is filled with deterministic real bytes — a
/// genuine over-cap binary payload the production externalize must store. Its size
/// guarantees the externalize policy (`> blob_inline_cap`) fires.
fn large_image_payload() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend((0..64 * 1024).map(|i: usize| (i.wrapping_mul(37).wrapping_add(11)) as u8));
    bytes
}

/// A `SurfaceDriver` that PANICS if reached: this `CallModel`-only loopback never
/// drives a surface, so a reached `drive` would be a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("the blob loopback drives only a CallModel; the surface driver must not be reached");
    }
}

/// A `ModelCaller` that forwards to the REAL selected provider and then attaches a
/// REAL large image payload to the response — exactly the shape a multimodal turn
/// (or a large image-bearing tool result folded into the response) produces. The
/// model CALL is real (text in/out); the attached image is the real over-cap payload
/// the production driver must externalize to the `BlobStore`. The request body lock
/// is dropped before the await so the future stays `Send`.
struct LargeImageCaller<'a> {
    inner: &'a dyn ModelApi,
    image_bytes: Vec<u8>,
    mime: String,
    last_request_had_image: Mutex<Option<bool>>,
}

impl<'a> LargeImageCaller<'a> {
    fn new(inner: &'a dyn ModelApi, image_bytes: Vec<u8>) -> Self {
        Self {
            inner,
            image_bytes,
            mime: "image/png".into(),
            last_request_had_image: Mutex::new(None),
        }
    }

    /// Whether the neutral request the live driver assembled and sent contained NO
    /// image block — the request is text-only (the over-cap payload is carried by the
    /// RESPONSE, so the real provider never has to accept an image, keeping the live
    /// call robust). `Some(false)` only if an image somehow appeared in the request.
    fn request_was_text_only(&self) -> Option<bool> {
        self.last_request_had_image
            .lock()
            .expect("lock the recorded request image flag")
            .map(|had_image| !had_image)
    }
}

impl ModelApi for LargeImageCaller<'_> {
    fn turn<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        let had_image = req
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, ContentBlock::Image { .. })));
        {
            let mut guard = self
                .last_request_had_image
                .lock()
                .expect("lock the recorded request image flag");
            *guard = Some(had_image);
        }
        let image_bytes = self.image_bytes.clone();
        let mime = self.mime.clone();
        let fut = self.inner.turn(req);
        Box::pin(async move {
            let mut resp = fut.await?;
            // Attach the REAL large image payload to the genuine model response — the
            // production externalize policy will `put` it to the BlobStore and rewrite
            // it to `ImageSource::Blob{hash}` because it is over the cap. The neutral
            // Base64 source round-trips to the world `Inline` bytes at the seam.
            resp.blocks.push(ContentBlock::Image {
                source: NeutralImageSource::Base64 {
                    media_type: mime,
                    data: base64::engine::general_purpose::STANDARD.encode(&image_bytes),
                },
            });
            Ok(resp)
        })
    }
    fn list_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelInfo>, Error>> + Send + 'a>> {
        self.inner.list_models()
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
}

// ---------------------------------------------------------------------------
// VC-1.4 entry point (dispatched from `tests/system/main.rs`)
// ---------------------------------------------------------------------------

/// VC-1.4 — drive a turn carrying a real large image over the cap through the live
/// driver against a real model, and prove the payload was externalized to the
/// `BlobStore` (the log holds the hash) and retrieved by hash (`BlobStore::get`).
pub async fn run() {
    if skip_without_live_llm("app::endurance_blob_loopback (VC-1.4)") {
        return;
    }

    let client = selected_from_env()
        .expect("build a provider from RUBBERDUX_LLM_* for the live blob turn");
    let model = env_model_config(client.model());

    let payload = large_image_payload();
    // Non-vacuity precondition: the payload is genuinely OVER the cap, so it MUST
    // externalize (not stay inline) — "the log holds the hash" cannot be satisfied by
    // an inline image.
    assert!(
        payload.len() > BLOB_INLINE_CAP as usize,
        "the test payload ({} bytes) must exceed blob_inline_cap ({} bytes) for a non-vacuous externalize",
        payload.len(),
        BLOB_INLINE_CAP
    );
    eprintln!(
        "[VC-1.4] real env model={:?}; payload={} bytes; blob_inline_cap={} bytes (payload MUST externalize)",
        model.model,
        payload.len(),
        BLOB_INLINE_CAP
    );

    // -- The durable, content-addressed BlobStore the turn externalizes into --------
    let blob_dir = tempfile::tempdir().expect("create a tempdir for the BlobStore");
    let store = BlobStore::new(blob_dir.path().join("blobs"));
    assert!(
        store.stored_hashes().expect("enumerate stored blobs").is_empty(),
        "the BlobStore starts empty (the externalize is the only writer)"
    );

    // -- Build the World and drive the primary's turn through the pure reducer -------
    let world = genesis(&model);
    let initiate = Event {
        origin: Origin::Human,
        edge: 0,
        at: 1,
        wall: None,
        input: LogicalInput::UserMessage {
            to: PRIMARY,
            text: "Reply with exactly one word: ACK.".into(),
        },
    };
    let (after_intake, commands) = rubberdux::agent::world::systems::tick(&world, &initiate);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, Command::CallModel { entity, .. } if *entity == PRIMARY)),
        "the primary's turn must emit a CallModel"
    );

    // -- Dispatch the emitted CallModel LIVE through the production blob-sink driver --
    let caller = LargeImageCaller::new(&client, payload.clone());
    let stamp = ResultStamp {
        edge: 0,
        app_edge: 1,
        at: 2,
        wall: None,
    };
    let mut log = MemoryEventLog::new();
    let results = drive_live_with_blob_sink(
        &commands,
        stamp,
        &after_intake,
        &caller,
        &NoSurfaceDrive,
        &UnattachedPeerSender,
        &store,
        &mut log,
    )
    .await
    .expect("the live blob-sink driver dispatches the turn and externalizes the payload");

    // -- The real model call succeeded (the live backbone) ---------------------------
    assert!(
        !results
            .iter()
            .any(|e| matches!(e.input, LogicalInput::ModelFailed { .. })),
        "VC-1.4: the real model call must succeed — no ModelFailed (results: {})",
        variant_dump(&results)
    );
    let responded = results
        .iter()
        .find_map(|e| match &e.input {
            LogicalInput::ModelResponded { entity, blocks, .. } => Some((*entity, blocks.clone())),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "VC-1.4: the turn must produce a REAL ModelResponded (results: {})",
                variant_dump(&results)
            )
        });
    let (responded_entity, blocks) = responded;
    assert_eq!(
        responded_entity, PRIMARY,
        "the recorded ModelResponded routes back to the primary (Inv 17)"
    );

    // The request the provider saw was text-only — the over-cap payload rode on the
    // RESPONSE, so the live call is robust against non-multimodal providers.
    if let Some(text_only) = caller.request_was_text_only() {
        assert!(
            text_only,
            "the live request body carried no image — the over-cap payload is on the response"
        );
    }

    // -- (1) The log holds the HASH, not the bytes -----------------------------------
    let hash = blocks
        .iter()
        .find_map(|b| match b {
            Block::Image {
                source: ImageSource::Blob { hash, mime },
            } => {
                assert_eq!(mime, "image/png", "the mime is preserved across externalize");
                Some(hash.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "VC-1.4: the over-cap image must be externalized to ImageSource::Blob{{hash}} \
                 (recorded blocks: {blocks:?})"
            )
        });
    // The recorded result Event serializes with the hash + a blob reference, never an
    // inline image carrying the over-cap bytes.
    let responded_event = results
        .iter()
        .find(|e| matches!(e.input, LogicalInput::ModelResponded { .. }))
        .expect("the ModelResponded event");
    let event_json = serde_json::to_string(responded_event).expect("serialize the ModelResponded");
    assert!(
        event_json.contains(&hash.0),
        "VC-1.4: the recorded log carries the blob content address"
    );
    assert!(
        event_json.contains("\"type\":\"blob\""),
        "VC-1.4: the over-cap image is recorded as a blob reference"
    );
    assert!(
        !event_json.contains("\"type\":\"inline\""),
        "VC-1.4: the log holds the HASH, never the inline bytes"
    );
    eprintln!(
        "[VC-1.4] the turn externalized the over-cap payload to ImageSource::Blob{{hash}} hash={}",
        hash.0
    );

    // -- (2) The bytes come from BlobStore::get (retrieved BY HASH) -------------------
    let stored = store.stored_hashes().expect("enumerate stored blobs");
    assert_eq!(
        stored,
        vec![hash.clone()],
        "VC-1.4: exactly the one externalized blob is stored, addressed by its hash"
    );
    let retrieved = store
        .get(&hash)
        .expect("VC-1.4: retrieve the externalized payload by hash from the BlobStore");
    assert_eq!(
        retrieved, payload,
        "VC-1.4: BlobStore::get(hash) returns the EXACT real payload bytes (round-trip by hash)"
    );

    // -- Persist the collected transcript for debugging (tests/CLAUDE.md) -------------
    let recorded = log.load().expect("load the dispatched result log");
    write_transcript(&recorded, &hash.0, payload.len());

    eprintln!(
        "[VC-1.4] PASS: a turn carrying a real {}-byte image (over the {}-byte cap) was \
         externalized on a LIVE model turn to the BlobStore as Blob{{hash={}}} — the log holds \
         the hash, and BlobStore::get returned the exact {} bytes by hash.",
        payload.len(),
        BLOB_INLINE_CAP,
        hash.0,
        retrieved.len()
    );
}

/// A compact `variant: count` dump of a result log, for self-explaining failure output.
fn variant_dump(events: &[Event]) -> String {
    events
        .iter()
        .map(|e| match &e.input {
            LogicalInput::ModelResponded { .. } => "ModelResponded",
            LogicalInput::ModelFailed { .. } => "ModelFailed",
            LogicalInput::UserMessage { .. } => "UserMessage",
            _ => "<other>",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The per-run results directory for the collected transcript, under
/// `tests/results/<unix-millis>/system/endurance_blob_loopback/` rooted at the
/// crate. A timestamped subdir keeps successive live runs from clobbering one another.
fn results_dir() -> std::path::PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("results")
        .join(format!("{millis}"))
        .join("system")
        .join("endurance_blob_loopback")
}

/// Write the dispatched result log as raw JSONL plus a short markdown narration, so a
/// failing live run can be inspected (tests/CLAUDE.md). The narration records the
/// externalized blob hash and the payload size, the load-bearing facts of this case.
fn write_transcript(events: &[Event], hash: &str, payload_len: usize) {
    let dir = results_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut md = String::from("# Endurance blob loopback — dispatched result transcript\n\n");
    md.push_str(&format!(
        "- externalized payload: {payload_len} bytes → BlobStore as Blob{{hash={hash}}}\n\n"
    ));
    for e in events {
        let variant = match &e.input {
            LogicalInput::ModelResponded { entity, .. } => {
                format!("ModelResponded(entity={entity})")
            }
            LogicalInput::ModelFailed { entity, .. } => format!("ModelFailed(entity={entity})"),
            other => format!("{other:?}"),
        };
        md.push_str(&format!(
            "- at={} edge={} origin={:?} — {variant}\n",
            e.at, e.edge, e.origin
        ));
    }
    let _ = std::fs::write(dir.join("narration.md"), md);
    if let Ok(serialized) = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
    {
        let _ = std::fs::write(dir.join("transcript.jsonl"), serialized.join("\n"));
    }
}
