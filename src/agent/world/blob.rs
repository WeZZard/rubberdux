// `BlobStore` is consumed by the inline/externalize policy (BL-cap) and the
// reachability GC (BL-gc); neither caller is wired yet, so the binary target has
// no caller for this API. Silence dead-code lints here, mirroring snapshot.rs.
#![allow(dead_code)]

//! Blob — a durable, append-only, content-addressed blob store.
//!
//! `BlobStore` is the **primary** home of externalized blob bytes (large
//! `Block::Image` / `Block::ToolResult` payloads): once a block carries only a
//! `BlobHash` reference, the bytes live ONLY here — they are not regenerable
//! from the log, so the store must be as durable as the log itself. Bytes are
//! keyed by the SHA-256 of their content, which gives automatic dedup (one
//! entry per hash) and tamper detection (bytes that no longer re-hash to their
//! address are corruption, caught on `get`).
//!
//! `put` is idempotent: identical bytes hash identically, so a second `put`
//! resolves to the same content address and the same `BlobHash` without
//! duplicating the file. Writes are durable: bytes go to a sibling temp file in
//! the SAME shard directory, are fsync'd, renamed into place, and the directory
//! entry is fsync'd before `put` returns — the temp→fsync→rename discipline of
//! `SnapshotStore`, so a crash never leaves a half-written blob at a content
//! address. The store performs blocking fsync'd I/O and is therefore meant to be
//! driven off the chat handler's critical path (BL-cap dispatches it as
//! background work).
//!
//! `stored_hashes` is the enumeration seam the reachability GC (BL-gc) builds
//! on: it lists every content address currently stored so an unreferenced blob
//! can be reclaimed. The GC itself lives here as the pure predicate
//! [`reachable_blobs`] (every hash any RETAINED log segment or snapshot
//! references) + [`reclaimable_blobs`] (`stored − reachable`) and the IO
//! [`sweep_blobs`]/[`BlobStore::remove`] shell that deletes ONLY reclaimable
//! blobs, NEVER a reachable one — mirroring the dead-branch reachability GC in
//! `reclaim.rs`.
//!
//! See docs/agent/world/ecs-runtime.md §1415-1434 (Large blobs — externalize to
//! bound log and snapshot size; content-addressed durable blob store = PRIMARY
//! STORAGE, not a derived cache) and §1426-1428 (GC by log-reachability).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::agent::world::history::{BlobHash, Block, ImageSource};
use crate::agent::world::inputs::{Event, LogicalInput};
use crate::agent::world::world::{Activity, World};
use crate::error::Error;

// ---------------------------------------------------------------------------
// Content address — the pure hash of the bytes
// ---------------------------------------------------------------------------

/// Compute the content address of `bytes`: `hex(SHA-256(bytes))`.
///
/// A pure function of the bytes — deterministic and content-addressed — reusing
/// the exact SHA-256-hex scheme of `effects::fingerprint_call` and
/// `snapshot::world_digest` (Inv 7) so a blob's address is a stable content hash
/// across the whole runtime. The result is the 64-char lowercase hex digest with
/// no algorithm prefix, which also makes the on-disk shard fan-out (below) even.
fn hash_bytes(bytes: &[u8]) -> BlobHash {
    BlobHash(hex::encode(Sha256::digest(bytes)))
}

/// Whether `s` is a canonical content address: a 64-char lowercase hex SHA-256
/// digest. Guards `get`/path-derivation against malformed `BlobHash`es (e.g.
/// from a tampered log) and filters stray files out of `stored_hashes`.
fn is_blob_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// BlobStore — directory-scoped, content-addressed, durable
// ---------------------------------------------------------------------------

/// A directory-scoped, content-addressed blob store.
///
/// Each blob is persisted at `<root>/<hash[0..2]>/<hash>` — the two-character
/// prefix fans the entries across `256` shard directories so no single
/// directory holds every blob. The file content is exactly the blob bytes; the
/// filename IS the content address.
///
/// See docs/agent/world/ecs-runtime.md §1415-1434.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Create a `BlobStore` rooted at `root`.
    ///
    /// The root and per-blob shard directories are created lazily on the first
    /// `put`, mirroring `SnapshotStore::new`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store `bytes` durably and return their content address.
    ///
    /// The address is `hex(SHA-256(bytes))` (pure, deterministic). The write is
    /// IDEMPOTENT: identical bytes resolve to the same path, so if a blob already
    /// exists at the content address this is a durable no-op that returns the
    /// same `BlobHash` without rewriting or duplicating the file.
    ///
    /// On a first write the bytes go to a sibling `<hash>.tmp` in the SAME shard
    /// directory (guaranteeing one filesystem for the rename), are fsync'd,
    /// renamed into place, and the shard directory is fsync'd so the new entry is
    /// durable before this returns. No `.tmp` is left behind on any error branch.
    ///
    /// See docs/agent/world/ecs-runtime.md §1415-1434 (durability is the blob
    /// store's own responsibility).
    pub fn put(&self, bytes: &[u8]) -> Result<BlobHash, Error> {
        let hash = hash_bytes(bytes);
        let shard = self.root.join(&hash.0[0..2]);
        let final_path = shard.join(&hash.0);

        // Idempotent dedup: identical bytes hash identically, so an existing file
        // at this content address already holds exactly these bytes (corruption
        // is caught by `get`). A repeat `put` is a no-op.
        if final_path.exists() {
            return Ok(hash);
        }

        std::fs::create_dir_all(&shard)?;

        // Atomic + durable publish: temp in the SAME shard → fsync file → rename
        // → fsync shard dir, the discipline `SnapshotStore::write` uses so a crash
        // never publishes a half-written blob at its content address.
        let tmp_path = shard.join(format!("{}.tmp", hash.0));
        let result = (|| -> Result<(), Error> {
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &final_path)?;
            // Fsync the shard directory so the rename (the new directory entry) is
            // itself durable, not just the file's data blocks.
            let dir = std::fs::File::open(&shard)?;
            dir.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            // Best-effort cleanup: leave no stray .tmp on any error branch.
            let _ = std::fs::remove_file(&tmp_path);
        }
        result?;
        Ok(hash)
    }

    /// Read the bytes stored at content address `hash`.
    ///
    /// Verifies tamper/corruption on read (the design's content-addressing
    /// guarantee): the bytes at a content address must re-hash to that address;
    /// a mismatch is corruption and returns `Error::World`. A missing blob
    /// surfaces the underlying `NotFound` as `Error::Io`.
    ///
    /// See docs/agent/world/ecs-runtime.md §1415-1434 (tamper detection on read).
    pub fn get(&self, hash: &BlobHash) -> Result<Vec<u8>, Error> {
        if !is_blob_hash(&hash.0) {
            return Err(Error::World(format!(
                "malformed blob hash {:?}: expected a 64-char lowercase hex SHA-256",
                hash.0
            )));
        }
        let path = self.root.join(&hash.0[0..2]).join(&hash.0);
        let bytes = std::fs::read(&path)?;
        let actual = hash_bytes(&bytes);
        if &actual != hash {
            return Err(Error::World(format!(
                "blob {} is corrupt: stored bytes hash to {}",
                hash.0, actual.0
            )));
        }
        Ok(bytes)
    }

    /// Enumerate every content address currently stored — the seam the
    /// reachability GC (BL-gc) builds on to find blobs no longer referenced by
    /// any retained log segment or snapshot.
    ///
    /// Walks the shard directories and returns the canonical content-address
    /// filenames, deterministically ordered (a `BTreeSet`, not a `HashMap`, so
    /// no iteration-order randomness). Non-canonical entries (`.tmp`, files
    /// whose name is not a hash or sits in the wrong shard) are ignored. A
    /// missing root yields an empty list.
    ///
    /// See docs/agent/world/ecs-runtime.md §1415-1434 (GC by log-reachability).
    pub fn stored_hashes(&self) -> Result<Vec<BlobHash>, Error> {
        let shards = match std::fs::read_dir(&self.root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(e)),
            Ok(entries) => entries,
        };
        let mut hashes = BTreeSet::new();
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            let shard_name = shard.file_name().to_string_lossy().into_owned();
            for blob in std::fs::read_dir(shard.path())? {
                let blob = blob?;
                let name = blob.file_name().to_string_lossy().into_owned();
                // Only canonical content addresses count, and the file must sit in
                // the shard its prefix names — a blob moved under the wrong shard
                // can't masquerade as a valid address.
                if is_blob_hash(&name) && name.starts_with(&shard_name) {
                    hashes.insert(BlobHash(name));
                }
            }
        }
        Ok(hashes.into_iter().collect())
    }

    /// Delete the blob stored at content address `hash` — the per-blob delete the
    /// reachability sweep ([`sweep_blobs`]) drives. A malformed `hash` is REFUSED
    /// (never derived into a path) so a sweep can never slice a sub-2-char string
    /// or resolve to a non-blob path. A missing file is an idempotent no-op (a
    /// prior sweep already removed it, or it was never stored), so re-sweeping the
    /// same set reports cleanly. This is the blob-store counterpart of
    /// `SnapshotStore::remove` and `reclaim::sweep`'s per-artifact delete.
    ///
    /// SAFETY: callers MUST only pass reclaimable hashes ([`reclaimable_blobs`]);
    /// `sweep_blobs` enforces that a still-reachable hash is never passed here.
    ///
    /// See docs/agent/world/ecs-runtime.md §1426-1428 (GC by log-reachability).
    pub fn remove(&self, hash: &BlobHash) -> Result<(), Error> {
        if !is_blob_hash(&hash.0) {
            return Err(Error::World(format!(
                "refusing to remove malformed blob hash {:?}: expected a 64-char lowercase hex SHA-256",
                hash.0
            )));
        }
        let path = self.root.join(&hash.0[0..2]).join(&hash.0);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already gone — idempotent (a prior sweep, or never stored).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Blob reachability GC — pure reachable/reclaimable + the sweep shell
// ---------------------------------------------------------------------------
//
// Mirrors the dead-branch reachability GC in `reclaim.rs`: a PURE predicate
// (no IO) computes the reachable set from the retained data, a PURE policy
// subtracts it from the stored set to get the reclaimable set, and a thin IO
// SHELL deletes only the reclaimable blobs — NEVER a reachable one. Losing a
// still-referenced blob is data loss (the bytes live ONLY in the store, not
// regenerable from the log — see this module's header), so the sweep treats
// "never delete a reachable hash" as its paramount invariant, defensively
// re-checked even though the reclaimable set is disjoint from reachable by
// construction. Reachability tracks RETAINED data (the live log segments + the
// retained snapshots), NOT all history ever written: a blob referenced only by
// a pruned/cold-dropped segment is correctly reclaimable.
//
// See docs/agent/world/ecs-runtime.md §1426-1428 (GC by log-reachability) and
// `reclaim.rs` (the dead-branch reachability GC this mirrors).

/// Collect every `BlobHash` an externalized image in `block` references, recursing
/// into `ToolResult` content — the SAME block shape BL-cap's externalize/resolve
/// walked (`effects::externalize_block`). Only `ImageSource::Blob` references the
/// store; an `Inline` image carries its own bytes and references no blob, and
/// `Text`/`ToolUse`/`Reasoning` carry none.
fn collect_block_blob_hashes(block: &Block, into: &mut BTreeSet<BlobHash>) {
    match block {
        Block::Image {
            source: ImageSource::Blob { hash, .. },
        } => {
            into.insert(hash.clone());
        }
        Block::ToolResult { content, .. } => {
            for inner in content {
                collect_block_blob_hashes(inner, into);
            }
        }
        // Text / ToolUse / Reasoning / Inline image reference no blob.
        _ => {}
    }
}

/// Collect blob hashes from a slice of blocks (each walked by
/// [`collect_block_blob_hashes`]).
fn collect_blocks_blob_hashes(blocks: &[Block], into: &mut BTreeSet<BlobHash>) {
    for block in blocks {
        collect_block_blob_hashes(block, into);
    }
}

/// Collect every blob hash referenced by one RETAINED log event.
///
/// Only the block-bearing result variants can carry an externalized image: the
/// BL-cap externalize path records `Blob{hash}` into `ModelResponded`/`Compacted`,
/// and a tool/child result may likewise carry one. Every other `LogicalInput`
/// carries no `Block` (text, JSON, surface ops, peer envelopes, acks), hence no
/// blob reference.
fn collect_event_blob_hashes(event: &Event, into: &mut BTreeSet<BlobHash>) {
    match &event.input {
        LogicalInput::ModelResponded { blocks, .. } => collect_blocks_blob_hashes(blocks, into),
        LogicalInput::ToolReturned { result, .. } => collect_blocks_blob_hashes(result, into),
        LogicalInput::ChildReturned { result, .. } => collect_block_blob_hashes(result, into),
        LogicalInput::Compacted { summary, .. } => collect_blocks_blob_hashes(summary, into),
        _ => {}
    }
}

/// Collect every blob hash referenced by a snapshot `World`: each entity's
/// History blocks, its steered-but-undrained inbox messages, and any in-flight
/// tool-slot result. Walks the same block shape as the log scan — a snapshot is
/// the folded image of the log, so a blob referenced by retained state must be
/// found here too.
fn collect_world_blob_hashes(world: &World, into: &mut BTreeSet<BlobHash>) {
    for components in world.entities.values() {
        for msg in components.history.messages() {
            collect_blocks_blob_hashes(&msg.content, into);
        }
        for pending in &components.inbox.pending {
            collect_blocks_blob_hashes(pending, into);
        }
        if let Activity::ResolvingToolUses { slots } = &components.activity {
            for slot in slots {
                if let Some(result) = &slot.result {
                    collect_block_blob_hashes(result, into);
                }
            }
        }
    }
}

/// PURE blob reachability: every `BlobHash` referenced by any RETAINED log
/// segment OR snapshot.
///
/// `segments` are the retained log events (the live hot + cold segments the
/// segmented log still holds); `snapshots` are the retained snapshot Worlds. The
/// scan walks each retained event's blocks and each snapshot World's blocks
/// (recursing into `ToolResult` content), collecting every `Block::Image{Blob}`
/// hash into a deterministically-ordered `BTreeSet` (never a `HashMap`, so no
/// iteration-order randomness leaks into the sweep).
///
/// This function is PURE: it performs NO IO, taking the retained segments and
/// snapshots as inputs and returning the reachable set — the blob-store mirror of
/// `reclaim::reachable`. Reachability tracks RETAINED data, NOT all history: a
/// blob referenced only by a PRUNED (non-retained) segment is absent here and is
/// therefore reclaimable.
///
/// See docs/agent/world/ecs-runtime.md §1426-1428 (GC by log-reachability).
pub fn reachable_blobs(segments: &[Event], snapshots: &[World]) -> BTreeSet<BlobHash> {
    let mut reachable = BTreeSet::new();
    for event in segments {
        collect_event_blob_hashes(event, &mut reachable);
    }
    for world in snapshots {
        collect_world_blob_hashes(world, &mut reachable);
    }
    reachable
}

/// PURE reclaimable policy: `stored − reachable` — the stored blobs no retained
/// segment or snapshot references.
///
/// A blob is reclaimable iff it is in the store ([`BlobStore::stored_hashes`]) but
/// NOT in the reachable set ([`reachable_blobs`]). No IO — the blob-store mirror
/// of `reclaim::reclaimable`. The result preserves `stored`'s (deterministic)
/// order so a sweep is reproducible.
///
/// See docs/agent/world/ecs-runtime.md §1426-1428 (GC by log-reachability).
pub fn reclaimable_blobs(stored: &[BlobHash], reachable: &BTreeSet<BlobHash>) -> Vec<BlobHash> {
    stored
        .iter()
        .filter(|hash| !reachable.contains(*hash))
        .cloned()
        .collect()
}

/// The sweep SHELL: delete each reclaimable blob from `store`, and NEVER a
/// reachable one.
///
/// `reachable` is passed only as a DEFENSIVE guard: `reclaimable` is already
/// `stored − reachable` ([`reclaimable_blobs`]), so the two sets are disjoint and
/// the guard can never fire on a correct caller — but losing a still-referenced
/// blob is irrecoverable data loss (the bytes live ONLY in the store), so the
/// shell makes "never delete a reachable hash" a hard invariant rather than a
/// derived property. This mirrors `reclaim::sweep`, which likewise guards the
/// shared/live artifact (the MAIN id) defensively before any delete. Returns the
/// hashes actually removed (a blob already gone is skipped, so reporting stays
/// idempotent across repeated sweeps).
///
/// Callers should only pass hashes returned by [`reclaimable_blobs`].
///
/// See docs/agent/world/ecs-runtime.md §1426-1428 (GC by log-reachability).
pub fn sweep_blobs(
    store: &BlobStore,
    reclaimable: &[BlobHash],
    reachable: &BTreeSet<BlobHash>,
) -> Result<Vec<BlobHash>, Error> {
    let mut swept = Vec::new();
    for hash in reclaimable {
        // SAFETY: never delete a still-referenced blob — that is data loss. The
        // reclaimable set is disjoint from `reachable` by construction; this guard
        // enforces the invariant regardless of how the set was computed.
        if reachable.contains(hash) {
            continue;
        }
        store.remove(hash)?;
        swept.push(hash.clone());
    }
    Ok(swept)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `put` then `get` returns the original bytes (round-trip).
    #[test]
    fn put_get_round_trip() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let bytes = b"the quick brown fox jumps over the lazy dog".to_vec();
        let hash = store.put(&bytes).expect("put must succeed");
        let read = store.get(&hash).expect("get must succeed");

        assert_eq!(read, bytes, "get must return the bytes that were put");
    }

    /// The address is the pure SHA-256-hex of the bytes — content-addressed and
    /// independent of the store instance.
    #[test]
    fn address_is_pure_content_hash() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let bytes = b"deterministic".to_vec();
        let hash = store.put(&bytes).expect("put must succeed");

        let expected = hex::encode(Sha256::digest(&bytes));
        assert_eq!(hash.0, expected, "address must be hex(SHA-256(bytes))");
        assert!(is_blob_hash(&hash.0), "address must be a 64-char lowercase hex digest");
    }

    /// `put` of identical bytes is idempotent: the same hash is returned and
    /// exactly one file is stored (dedup), no duplicate.
    #[test]
    fn put_is_idempotent_and_dedups() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let bytes = b"store me once".to_vec();
        let h1 = store.put(&bytes).expect("first put");
        let h2 = store.put(&bytes).expect("second put of identical bytes");

        assert_eq!(h1, h2, "identical bytes must yield the same hash");

        // Exactly one content address is stored — no duplication, no .tmp leak.
        let stored = store.stored_hashes().expect("stored_hashes");
        assert_eq!(stored, vec![h1.clone()], "identical bytes must store exactly one blob");

        // And the single file still round-trips.
        assert_eq!(store.get(&h1).expect("get"), bytes);
    }

    /// Distinct bytes produce distinct hashes and distinct stored blobs.
    #[test]
    fn distinct_bytes_distinct_hashes() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let a = store.put(b"alpha").expect("put alpha");
        let b = store.put(b"beta").expect("put beta");

        assert_ne!(a, b, "distinct bytes must hash to distinct addresses");

        let mut stored = store.stored_hashes().expect("stored_hashes");
        stored.sort();
        let mut expected = vec![a.clone(), b.clone()];
        expected.sort();
        assert_eq!(stored, expected, "both distinct blobs must be enumerated");

        assert_eq!(store.get(&a).expect("get a"), b"alpha");
        assert_eq!(store.get(&b).expect("get b"), b"beta");
    }

    /// Blobs land at the fan-out path `<root>/<hash[0..2]>/<hash>`.
    #[test]
    fn on_disk_layout_fans_out_by_prefix() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path().join("blobs");
        let store = BlobStore::new(&root);

        let hash = store.put(b"layout").expect("put");
        let expected = root.join(&hash.0[0..2]).join(&hash.0);
        assert!(expected.exists(), "blob must live at <root>/<hash[0..2]>/<hash>");

        // No .tmp file is left behind after a successful put.
        let shard = root.join(&hash.0[0..2]);
        let tmp_left: Vec<_> = std::fs::read_dir(&shard)
            .expect("read shard dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(tmp_left.is_empty(), "no .tmp file must remain: {tmp_left:?}");
    }

    /// `get` of an absent blob is an error (not a panic).
    #[test]
    fn get_missing_blob_errors() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        // A well-formed but never-stored address.
        let absent = hash_bytes(b"never stored");
        assert!(store.get(&absent).is_err(), "missing blob must error");
    }

    /// `get` of a malformed hash errors rather than slicing/panicking.
    #[test]
    fn get_malformed_hash_errors() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        assert!(store.get(&BlobHash("x".into())).is_err(), "short hash must error");
        assert!(
            store.get(&BlobHash("sha256:abc".into())).is_err(),
            "non-hex hash must error"
        );
    }

    /// A blob whose bytes were tampered no longer re-hashes to its address and is
    /// reported as corruption on `get` (content-addressing tamper detection).
    #[test]
    fn get_detects_corruption() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let root = dir.path().join("blobs");
        let store = BlobStore::new(&root);

        let hash = store.put(b"honest bytes").expect("put");
        let path = root.join(&hash.0[0..2]).join(&hash.0);

        // Tamper with the stored bytes in place.
        std::fs::write(&path, b"tampered bytes").expect("overwrite blob");

        assert!(
            store.get(&hash).is_err(),
            "corrupt bytes that don't re-hash to the address must error on get"
        );
    }

    /// `stored_hashes` on an empty/absent store is the empty list.
    #[test]
    fn stored_hashes_empty_when_absent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let store = BlobStore::new(dir.path().join("does-not-exist"));
        assert!(store.stored_hashes().expect("stored_hashes").is_empty());
    }

    // -----------------------------------------------------------------------
    // Blob reachability GC — pure reachable/reclaimable + the sweep shell
    // -----------------------------------------------------------------------

    use crate::agent::world::budget::Budget;
    use crate::agent::world::gates::EntityGate;
    use crate::agent::world::history::{History, Msg, Role};
    use crate::agent::world::inputs::{Fingerprint, Origin};
    use crate::agent::world::world::{
        Components, Effort, Identity, Inbox, Lineage, ModelConfig, Resources,
    };

    /// A `Block::Image` referencing the externalized blob at `hash`.
    fn blob_image(hash: &BlobHash) -> Block {
        Block::Image {
            source: ImageSource::Blob {
                hash: hash.clone(),
                mime: "image/png".into(),
            },
        }
    }

    /// A retained log event (a `ToolReturned`) whose result references `hash`.
    fn tool_returned_referencing(hash: &BlobHash) -> Event {
        Event {
            origin: Origin::Agent,
            edge: 0,
            at: 1,
            wall: None,
            input: LogicalInput::ToolReturned {
                cmd: 1,
                entity: 0,
                fingerprint: Fingerprint("fp".into()),
                result: vec![blob_image(hash)],
            },
        }
    }

    /// A snapshot World whose single entity's History references `hash`.
    fn world_referencing(hash: &BlobHash) -> World {
        let model = ModelConfig {
            model: "claude-blob-gc-unit".into(),
            max_tokens: 16,
            effort: Effort::Low,
        };
        let mut world = World::new(0, Resources::new(7, model));
        world.entities.insert(
            0,
            Components {
                identity: Identity::Primary,
                lineage: Lineage { parent: None, depth: 0 },
                history: History(vec![Msg {
                    role: Role::User,
                    content: vec![blob_image(hash)],
                }]),
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

    /// (a) A blob referenced by a retained log segment is reachable and is KEPT —
    /// it is never reclaimable and the sweep does not touch it.
    #[test]
    fn referenced_blob_is_reachable_and_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));
        let hash = store.put(b"referenced bytes").expect("put");

        let segments = vec![tool_returned_referencing(&hash)];
        let reachable = reachable_blobs(&segments, &[]);
        assert!(reachable.contains(&hash), "a referenced blob must be reachable");

        let stored = store.stored_hashes().expect("stored_hashes");
        let reclaimable = reclaimable_blobs(&stored, &reachable);
        assert!(
            !reclaimable.contains(&hash),
            "a reachable blob must NOT be reclaimable"
        );

        let swept = sweep_blobs(&store, &reclaimable, &reachable).expect("sweep");
        assert!(swept.is_empty(), "nothing is swept when the only blob is referenced");
        assert_eq!(
            store.get(&hash).expect("get"),
            b"referenced bytes",
            "the referenced blob must survive the sweep"
        );
    }

    /// A blob referenced only by a retained SNAPSHOT World (not by any log event)
    /// is reachable — the scan walks snapshot Worlds the same way as log segments.
    #[test]
    fn blob_referenced_only_by_snapshot_world_is_reachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));
        let hash = store.put(b"snapshot bytes").expect("put");

        let worlds = vec![world_referencing(&hash)];
        let reachable = reachable_blobs(&[], &worlds);
        assert!(
            reachable.contains(&hash),
            "a blob referenced by a snapshot World must be reachable"
        );
    }

    /// Reachability recurses into `ToolResult` content (the same nesting BL-cap's
    /// externalize walked): a blob nested inside a `ChildReturned` ToolResult is
    /// reachable.
    #[test]
    fn reachability_recurses_into_tool_result_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));
        let nested = store.put(b"nested in tool result").expect("put");

        let event = Event {
            origin: Origin::Agent,
            edge: 0,
            at: 2,
            wall: None,
            input: LogicalInput::ChildReturned {
                parent: 0,
                child: 1,
                tool_use_id: "tu".into(),
                result: Block::ToolResult {
                    tool_use_id: "tu".into(),
                    content: vec![blob_image(&nested)],
                    is_error: false,
                },
            },
        };
        let reachable = reachable_blobs(&[event], &[]);
        assert!(
            reachable.contains(&nested),
            "a blob nested in ToolResult content must be reachable"
        );
    }

    /// (b) A stored blob no retained segment/snapshot references is reclaimable and
    /// is swept; the referenced blob beside it survives.
    #[test]
    fn unreferenced_blob_is_reclaimable_and_swept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let referenced = store.put(b"keep me").expect("put referenced");
        let orphan = store.put(b"delete me").expect("put orphan");

        // Only `referenced` is referenced by retained data.
        let segments = vec![tool_returned_referencing(&referenced)];
        let reachable = reachable_blobs(&segments, &[]);
        let stored = store.stored_hashes().expect("stored_hashes");
        let reclaimable = reclaimable_blobs(&stored, &reachable);

        assert!(reclaimable.contains(&orphan), "the unreferenced blob is reclaimable");
        assert!(
            !reclaimable.contains(&referenced),
            "the referenced blob is not reclaimable"
        );

        let swept = sweep_blobs(&store, &reclaimable, &reachable).expect("sweep");
        assert_eq!(swept, vec![orphan.clone()], "only the orphan is swept");

        assert!(store.get(&orphan).is_err(), "the orphan blob must be deleted");
        assert_eq!(
            store.get(&referenced).expect("get referenced"),
            b"keep me",
            "the referenced blob must survive"
        );
        assert_eq!(
            store.stored_hashes().expect("stored_hashes after sweep"),
            vec![referenced],
            "only the referenced blob remains on disk"
        );
    }

    /// (c) NON-VACUITY: a blob referenced only by a PRUNED (non-retained) segment
    /// is reclaimable — reachability tracks RETAINED data, not all history ever
    /// written. The pruned segment is simply absent from the scan's inputs.
    #[test]
    fn blob_referenced_only_by_pruned_segment_is_reclaimable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        let retained_hash = store.put(b"in retained segment").expect("put retained");
        let pruned_hash = store.put(b"in pruned segment").expect("put pruned");

        // The full history once referenced BOTH; only the RETAINED segment is fed
        // to the scan. The pruned segment (and its blob reference) is gone.
        let retained_segments = vec![tool_returned_referencing(&retained_hash)];

        let reachable = reachable_blobs(&retained_segments, &[]);
        assert!(
            reachable.contains(&retained_hash),
            "the retained-segment blob is reachable"
        );
        assert!(
            !reachable.contains(&pruned_hash),
            "a blob referenced only by a pruned segment is NOT reachable \
             (reachability tracks retained data, not history)"
        );

        let stored = store.stored_hashes().expect("stored_hashes");
        let reclaimable = reclaimable_blobs(&stored, &reachable);
        assert!(reclaimable.contains(&pruned_hash), "the pruned-segment blob is reclaimable");
        assert!(
            !reclaimable.contains(&retained_hash),
            "the retained-segment blob is not reclaimable"
        );

        let swept = sweep_blobs(&store, &reclaimable, &reachable).expect("sweep");
        assert_eq!(swept, vec![pruned_hash.clone()], "only the pruned-segment blob is swept");
        assert!(store.get(&pruned_hash).is_err(), "the pruned-segment blob is deleted");
        assert_eq!(
            store.get(&retained_hash).expect("get retained"),
            b"in retained segment",
            "the retained-segment blob survives"
        );
    }

    /// The sweep's paramount invariant: it NEVER deletes a reachable hash, even if
    /// a (buggy) caller mistakenly lists one as reclaimable. The defensive guard
    /// refuses it — a referenced blob lost is irrecoverable data loss.
    #[test]
    fn sweep_never_deletes_a_reachable_blob_even_if_misclassified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));
        let hash = store.put(b"must survive").expect("put");

        let mut reachable = BTreeSet::new();
        reachable.insert(hash.clone());

        // Deliberately mis-pass the reachable hash as reclaimable.
        let swept = sweep_blobs(&store, &[hash.clone()], &reachable).expect("sweep");
        assert!(swept.is_empty(), "the sweep must NEVER delete a reachable blob");
        assert_eq!(
            store.get(&hash).expect("get"),
            b"must survive",
            "the reachable blob must survive a misclassified sweep"
        );
    }

    /// `remove` on a malformed hash is refused (no path derived from a non-hash),
    /// and `remove` of an absent blob is an idempotent no-op (re-sweeps stay clean).
    #[test]
    fn remove_refuses_malformed_and_is_idempotent_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::new(dir.path().join("blobs"));

        assert!(store.remove(&BlobHash("x".into())).is_err(), "malformed hash must be refused");

        // A well-formed but never-stored address removes cleanly (idempotent).
        let absent = hash_bytes(b"never stored");
        assert!(store.remove(&absent).is_ok(), "removing an absent blob is a no-op");
    }
}
