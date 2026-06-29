//! VC-1.1 / VC-1.3 — blob externalization + reachability GC, OFFLINE (deterministic).
//!
//! This is the BL-sink's OFFLINE half: it proves — without a single model call,
//! against the REAL content-addressed `BlobStore` and the REAL replay/GC spine —
//! the four deterministic blob properties US-1/US-2 promise:
//!
//! - **VC-1.1 (round-trip + dedup).** A real large payload externalized through the
//!   `BlobStore` round-trips BY HASH (`put → Blob{hash} → get` returns the exact
//!   bytes), and identical bytes are stored ONCE (a repeat `put` is an idempotent
//!   no-op — one stored content address, no duplicate).
//! - **VC-1.1 (byte-identical replay).** A recorded session whose `ModelResponded`
//!   carries an EXTERNALIZED `ImageSource::Blob{hash}` (the log holds the HASH,
//!   never the bytes) replays BYTE-IDENTICALLY: `replay_branch` reuses the recorded
//!   result with ZERO model calls (an [`ExplodingClient`] makes that a STRUCTURAL
//!   guarantee) and reconstructs a World byte-for-byte equal to `fold_log`. It is
//!   NON-VACUOUS: the same session with the image INLINED folds to a DIFFERENT
//!   (larger, byte-bearing) World, so the byte-identity is genuinely over the
//!   hash-referenced form — an inlined blob would diverge.
//! - **VC-1.3 (GC keep/reclaim).** Reachability GC over the retained segments keeps
//!   a STILL-REFERENCED blob and reclaims an UNREFERENCED one — mirroring the
//!   dead-branch reclaim proof. It is NON-VACUOUS: a no-op GC (sweeping nothing)
//!   would leave the orphan on disk and fail the reclaim assertions, and the pure
//!   `reachable`/`reclaimable` predicates partition referenced from unreferenced.
//!
//! It touches no `src/agent/world/*` file (it consumes `BlobStore`, the BL-gc
//! reachability predicates, the `reclaim_blobs` composed pass, and the replay spine
//! as a library) and makes no live model call, so it runs on every developer
//! machine. The live half (a real large payload externalized + retrieved on a real
//! model turn) is proven by `tests/system/app/endurance_blob_loopback.rs` (VC-1.4).
//!
//! See docs/agent/world/ecs-runtime.md §1415-1434 (content-addressed blob store =
//! PRIMARY storage), §1426-1428 (GC by log-reachability), Inv 6 (replay
//! determinism), Inv 7 (content-addressed replay — the fingerprint).

use std::collections::BTreeMap;

use serde_json::Value as Json;

use rubberdux::agent::world::blob::{reachable_blobs, reclaimable_blobs, BlobStore};
use rubberdux::agent::world::budget::Budget;
use rubberdux::agent::world::effects::{fingerprint_call, Command, SurfaceDriver};
use rubberdux::provider::{ModelApi, ModelInfo, ModelRequest, ModelResponse};
use std::future::Future;
use std::pin::Pin;

use rubberdux::agent::world::event_log::{EventLog, MemoryEventLog};
use rubberdux::agent::world::gates::EntityGate;
use rubberdux::agent::world::history::{BlobHash, Block, History, ImageSource};
use rubberdux::agent::world::inputs::{
    Capabilities, Event, Fingerprint, LogicalInput, ModelMeta, Origin, ReasoningPolicy, StopReason,
    Usage,
};
use rubberdux::agent::world::reclaim::reclaim_blobs;
use rubberdux::agent::world::replay::{
    fold_log, genesis_from_log, is_model_call_result, replay_branch,
};
use rubberdux::agent::world::systems::tick;
use rubberdux::agent::world::world::{
    Activity, CmdId, Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources, World,
};
use rubberdux::error::Error;

// The hidden RNG seed crossing the recorded boundary (Inv 8): inert here (no System
// draws) but it must reseed identically for the folds to match.
const SEED: u64 = 7;

/// The world-default `ModelConfig`. Its `model` id rides in the request the
/// re-emitted `CallModel` fingerprints, so the recorded `ModelResponded` below is
/// stamped against THIS value; no real call is ever made.
fn offline_model() -> ModelConfig {
    ModelConfig {
        model: "claude-blob-offline".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// The fresh `World` a session starts from: tick 0, a single primary `Idle` root
/// entity, `Resources` reseeded from `seed`. Mirrors the genesis every replay
/// harness builds (the shell owns genesis; no System creates it).
fn genesis(seed: u64, model: &ModelConfig) -> World {
    let mut world = World::new(0, Resources::new(seed, model.clone()));
    world.entities.insert(
        0,
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

// ---------------------------------------------------------------------------
// Recorded-log builders — keep the hand-authored session readable
// ---------------------------------------------------------------------------

fn session_started() -> Event {
    Event {
        origin: Origin::System,
        edge: 0,
        at: 0,
        wall: None,
        input: LogicalInput::SessionStarted {
            seed: SEED,
            surface_tools: Vec::new(),
        },
    }
}

fn user_message(at: u64, text: &str) -> Event {
    Event {
        origin: Origin::Human,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::UserMessage {
            to: 0,
            text: text.into(),
        },
    }
}

/// A `ModelResponded` carrying the given response `blocks`. The `fingerprint` is
/// left empty here and stamped by [`stamp_fingerprints`] to the hash the live
/// driver would have recorded for the request the reducer re-emits for `cmd`.
fn model_responded_blocks(at: u64, cmd: CmdId, blocks: Vec<Block>) -> Event {
    Event {
        origin: Origin::Agent,
        edge: 0,
        at,
        wall: None,
        input: LogicalInput::ModelResponded {
            cmd,
            entity: 0,
            fingerprint: Fingerprint(String::new()),
            blocks,
            meta: ModelMeta {
                usage: Usage::default(),
                model_id: "claude-blob-offline".into(),
                stop_reason: StopReason::EndTurn,
                capabilities: Capabilities(serde_json::json!({})),
                reasoning: ReasoningPolicy::Drop,
            },
        },
    }
}

/// A `Block::Image` referencing the externalized blob at `hash` (the log/snapshot
/// form — only the content address, never the bytes).
fn blob_image(hash: &BlobHash) -> Block {
    Block::Image {
        source: ImageSource::Blob {
            hash: hash.clone(),
            mime: "image/png".into(),
        },
    }
}

/// An INLINE `Block::Image` carrying `bytes` (the pre-externalize / non-vacuity
/// foil form — the bytes live IN the block, not behind a hash).
fn inline_image(bytes: &[u8]) -> Block {
    Block::Image {
        source: ImageSource::Inline {
            mime: "image/png".into(),
            bytes: bytes.to_vec(),
        },
    }
}

/// Stamp each model-call result's `fingerprint` with the hash the LIVE driver would
/// record for the request the reducer re-emits for that `cmd`, so a faithful replay
/// REUSES the result instead of diverging (Inv 7). Mirrors the promoted spine's
/// recording discipline (`effects::fingerprint_call`) and the counterfactual sink.
fn stamp_fingerprints(events: &mut [Event], model: &ModelConfig) {
    let mut world = genesis_from_log(events, |seed| genesis(seed, model))
        .expect("genesis for the fingerprint pass");
    let mut by_cmd: BTreeMap<CmdId, Fingerprint> = BTreeMap::new();
    for ev in events.iter() {
        let (next, commands) = tick(&world, ev);
        world = next;
        for command in &commands {
            if let Command::CallModel {
                cmd,
                messages,
                tools,
                params,
                ..
            } = command
            {
                let fp = fingerprint_call(messages, tools, params).expect("fingerprint a request");
                by_cmd.insert(*cmd, fp);
            }
        }
    }
    for ev in events.iter_mut() {
        if let LogicalInput::ModelResponded {
            cmd, fingerprint, ..
        } = &mut ev.input
            && let Some(fp) = by_cmd.get(cmd)
        {
            *fingerprint = fp.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// ExplodingClient / NoSurfaceDrive — the structural zero-call witnesses
// ---------------------------------------------------------------------------

/// A `ModelCaller` that PANICS if invoked. Threaded into the offline `replay_branch`
/// so a single live call aborts the test — the structural proof that the recorded
/// externalized-blob result is REUSED with ZERO model calls (Inv 6).
struct ExplodingClient;

impl ModelApi for ExplodingClient {
    fn turn<'a>(
        &'a self,
        _req: &'a ModelRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ModelResponse, Error>> + Send + 'a>> {
        panic!("the offline blob replay must never invoke the model client");
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

/// A `SurfaceDriver` that PANICS if reached: this model-only session never drives a
/// surface, so a reached `drive` would be a structural error.
struct NoSurfaceDrive;

impl SurfaceDriver for NoSurfaceDrive {
    async fn drive(&self, _command: &Command) -> Result<(), Error> {
        panic!("a CallModel-only blob session must not reach the surface driver");
    }
}

// ---------------------------------------------------------------------------
// VC-1.1 — externalize round-trip by hash + identical bytes stored once
// ---------------------------------------------------------------------------

/// VC-1.1: a real large payload externalized through the `BlobStore` round-trips by
/// hash — `put → Blob{hash} → get` returns the exact bytes — and identical bytes are
/// stored ONCE (a repeat `put` is an idempotent no-op: one content address on disk,
/// no duplicate). This pins the content-addressed externalize the log/snapshot store
/// only the hash for.
#[test]
fn externalized_payload_round_trips_by_hash_and_identical_bytes_store_once() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = BlobStore::new(dir.path().join("blobs"));

    // A real, large payload (64 KiB) — far over any sane inline cap, so a session
    // running with a positive `blob_inline_cap` would externalize exactly this.
    let payload: Vec<u8> = (0..64 * 1024).map(|i| (i * 31 + 7) as u8).collect();

    // Externalize → the content address. The log/snapshot would hold ONLY this hash.
    let hash = store.put(&payload).expect("externalize the large payload");

    // Round-trip BY HASH: get(hash) returns exactly the bytes that were put.
    assert_eq!(
        store.get(&hash).expect("get the blob by hash"),
        payload,
        "VC-1.1: put → Blob{{hash}} → get round-trips the exact bytes by hash"
    );

    // Idempotent dedup: putting the IDENTICAL bytes again resolves to the SAME hash
    // and stores exactly ONE content address — no duplicate file.
    let hash_again = store.put(&payload).expect("re-put identical bytes");
    assert_eq!(
        hash, hash_again,
        "VC-1.1: identical bytes hash identically (content-addressed)"
    );
    assert_eq!(
        store.stored_hashes().expect("enumerate stored blobs"),
        vec![hash.clone()],
        "VC-1.1: identical bytes are stored ONCE (idempotent dedup — one blob on disk)"
    );

    // The single stored blob still round-trips after the dedup put.
    assert_eq!(
        store.get(&hash).expect("get after dedup"),
        payload,
        "VC-1.1: the deduplicated blob still returns the original bytes"
    );
}

// ---------------------------------------------------------------------------
// VC-1.1 — an externalized-blob log replays BYTE-IDENTICALLY (exploding client)
// ---------------------------------------------------------------------------

/// VC-1.1: a recorded session whose `ModelResponded` carries an EXTERNALIZED
/// `ImageSource::Blob{hash}` (the log holds the HASH, never the bytes) replays
/// BYTE-IDENTICALLY — `replay_branch` reuses the recorded result with ZERO model
/// calls (the [`ExplodingClient`] makes that structural) and reconstructs a World
/// byte-for-byte equal to `fold_log`.
///
/// NON-VACUOUS: the SAME session with the image INLINED folds to a DIFFERENT World
/// (the inline bytes live in the World, the hash form does not), so the recorded /
/// replayed byte-identity is genuinely over the hash-referenced form — an inlined
/// blob would diverge.
#[tokio::test]
async fn externalized_blob_log_replays_byte_identically_with_zero_model_calls() {
    let model = offline_model();

    let dir = tempfile::tempdir().expect("create tempdir");
    let store = BlobStore::new(dir.path().join("blobs"));
    // The real large payload a turn externalized; the store holds the bytes, the log
    // holds only the hash this `put` returns.
    let payload: Vec<u8> = (0..32 * 1024).map(|i| (i * 17 + 3) as u8).collect();
    let hash = store.put(&payload).expect("externalize the turn's image payload");

    // A one-turn session whose ModelResponded carries the externalized Blob{hash}.
    let mut log = vec![
        session_started(),
        user_message(1, "describe the attached image"),
        model_responded_blocks(
            2,
            0,
            vec![
                Block::Text {
                    text: "here is the image".into(),
                },
                blob_image(&hash),
            ],
        ),
    ];
    stamp_fingerprints(&mut log, &model);

    // The recorded log holds the HASH as a blob reference — never inline bytes.
    let log_json = serde_json::to_string(&log).expect("serialize the recorded log");
    assert!(
        log_json.contains(&hash.0),
        "the recorded log carries the blob content address"
    );
    assert!(
        log_json.contains("\"type\":\"blob\""),
        "the recorded image is a blob reference"
    );
    assert!(
        !log_json.contains("\"type\":\"inline\""),
        "no inline bytes are recorded in the log (only the hash)"
    );

    // The canonical fold the replay must reproduce byte-for-byte.
    let folded = fold_log(
        genesis_from_log(&log, |seed| genesis(seed, &model)).expect("genesis"),
        &log,
    );

    // Replay the externalized-blob log: the recorded ModelResponded is REUSED (its
    // fingerprint matches the re-emitted CallModel), so the ExplodingClient never
    // fires — ZERO model calls — and nothing is appended to the branch log.
    let mut branch_log = MemoryEventLog::new();
    let replayed = replay_branch(
        genesis_from_log(&log, |seed| genesis(seed, &model)).expect("genesis"),
        &log,
        is_model_call_result,
        &ExplodingClient,
        &NoSurfaceDrive,
        &mut branch_log,
    )
    .await
    .expect("the externalized-blob log replays, reusing the recorded result");

    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replayed World"),
        serde_json::to_vec(&folded).expect("serialize folded World"),
        "VC-1.1: the externalized-blob log replays BYTE-IDENTICALLY (Inv 6)"
    );
    assert_eq!(
        replayed, folded,
        "VC-1.1: replay_branch reconstructs the same World fold_log does"
    );
    assert!(
        branch_log.load().expect("load the branch log").is_empty(),
        "VC-1.1: zero live results appended — the externalized result was reused, not re-run"
    );

    // The reconstructed World references the blob by HASH, not by inline bytes.
    let folded_json = serde_json::to_string(&folded).expect("serialize the folded World");
    assert!(
        folded_json.contains(&hash.0),
        "the folded World references the blob by content address"
    );
    assert!(
        !folded_json.contains("\"type\":\"inline\""),
        "the folded World carries no inline image bytes (only the hash)"
    );

    // NON-VACUITY: the SAME session with the image INLINED folds to a DIFFERENT World
    // — the inline bytes live in the World, the hash form does not. So the
    // byte-identity above is genuinely over the externalized (hash) form: had the
    // blob been inlined, the recorded/replayed World would diverge.
    let mut inline_log = vec![
        session_started(),
        user_message(1, "describe the attached image"),
        model_responded_blocks(
            2,
            0,
            vec![
                Block::Text {
                    text: "here is the image".into(),
                },
                inline_image(&payload),
            ],
        ),
    ];
    stamp_fingerprints(&mut inline_log, &model);
    let inline_folded = fold_log(
        genesis_from_log(&inline_log, |seed| genesis(seed, &model)).expect("genesis"),
        &inline_log,
    );
    assert_ne!(
        serde_json::to_vec(&inline_folded).expect("serialize inline-folded World"),
        serde_json::to_vec(&folded).expect("serialize folded World"),
        "VC-1.1 (non-vacuity): an INLINED blob folds to a DIFFERENT World — the \
         byte-identity is genuinely over the hash-referenced form"
    );
    assert!(
        serde_json::to_string(&inline_folded)
            .expect("serialize inline-folded World")
            .contains("\"type\":\"inline\""),
        "the inline foil World genuinely carries the inline bytes (so the divergence is real)"
    );
}

// ---------------------------------------------------------------------------
// VC-1.3 — reachability GC keeps a referenced blob, reclaims an unreferenced one
// ---------------------------------------------------------------------------

/// VC-1.3: reachability GC over the RETAINED log segments keeps a STILL-REFERENCED
/// blob and reclaims an UNREFERENCED one — the blob-store mirror of the dead-branch
/// reachability GC. The pure `reachable`/`reclaimable` predicates partition
/// referenced from unreferenced, and the composed `reclaim_blobs` sweep deletes
/// exactly the orphan while the referenced blob survives and still round-trips.
///
/// NON-VACUOUS: a no-op GC (sweeping nothing) would leave the orphan on disk and
/// fail the "orphan get errors" + "only the referenced blob remains" assertions;
/// the reachable/reclaimable split distinguishes the two.
#[test]
fn gc_keeps_referenced_blob_and_reclaims_unreferenced() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = BlobStore::new(dir.path().join("blobs"));

    // Two distinct real payloads in the store: one a retained segment references,
    // one referenced by nothing retained (an orphan).
    let referenced_bytes: Vec<u8> = (0..8 * 1024).map(|i| (i * 13 + 1) as u8).collect();
    let orphan_bytes: Vec<u8> = (0..8 * 1024).map(|i| (i * 29 + 5) as u8).collect();
    let referenced = store.put(&referenced_bytes).expect("put the referenced blob");
    let orphan = store.put(&orphan_bytes).expect("put the orphan blob");
    assert_ne!(referenced, orphan, "the two payloads hash distinctly");

    // A retained log segment references ONLY `referenced` (a ModelResponded carrying
    // its Blob image); nothing retained references `orphan`.
    let segments = vec![model_responded_blocks(2, 0, vec![blob_image(&referenced)])];
    let snapshots: Vec<World> = Vec::new();

    // Pure reachability partitions referenced from unreferenced (no IO in the predicate).
    let reachable = reachable_blobs(&segments, &snapshots);
    assert!(
        reachable.contains(&referenced),
        "VC-1.3: a blob a retained segment references is reachable"
    );
    assert!(
        !reachable.contains(&orphan),
        "VC-1.3: a blob nothing retained references is NOT reachable"
    );

    let stored = store.stored_hashes().expect("enumerate stored blobs");
    let reclaimable = reclaimable_blobs(&stored, &reachable);
    assert_eq!(
        reclaimable,
        vec![orphan.clone()],
        "VC-1.3: only the unreferenced blob is reclaimable (stored − reachable)"
    );

    // The composed reclaim pass sweeps exactly the orphan and keeps the referenced blob.
    let swept = reclaim_blobs(&store, &segments, &snapshots).expect("reclaim_blobs");
    assert_eq!(
        swept,
        vec![orphan.clone()],
        "VC-1.3: GC reclaims EXACTLY the unreferenced blob"
    );

    // KEEP: the referenced blob survives and still round-trips by hash.
    assert_eq!(
        store.get(&referenced).expect("get the referenced blob"),
        referenced_bytes,
        "VC-1.3: the still-referenced blob is KEPT (and round-trips after the sweep)"
    );
    // RECLAIM: the orphan is gone.
    assert!(
        store.get(&orphan).is_err(),
        "VC-1.3: the unreferenced blob is RECLAIMED (deleted)"
    );
    assert_eq!(
        store.stored_hashes().expect("enumerate stored blobs after sweep"),
        vec![referenced],
        "VC-1.3: only the referenced blob remains on disk"
    );

    // NON-VACUITY: a second sweep over the same retained data is a clean no-op — the
    // referenced blob is never touched and the orphan is already gone (idempotent).
    let swept_again = reclaim_blobs(&store, &segments, &snapshots).expect("reclaim_blobs again");
    assert!(
        swept_again.is_empty(),
        "VC-1.3: re-sweeping reclaims nothing — the referenced blob is never reclaimed"
    );
}
