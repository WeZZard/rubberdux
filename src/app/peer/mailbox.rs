//! The per-App inbox for peer messages addressed to an offline App.
//!
//! When the broker resolves a [`PeerId`](crate::app::peer::PeerId) whose worker
//! is not running, it queues the message envelope to that App's `inbox.jsonl`
//! and wakes the App. When a tombstoned or human-archived App is restored, its
//! inbox is drained in order and each envelope is delivered to the freshly
//! started worker. Whether an App is offline because it was tombstoned (an idle
//! optimization) or human-archived (a decluttering choice) is irrelevant here:
//! both are still addressable, and both drain on restore.
//!
//! The inbox is an append-only JSONL log, mirroring the App registry's
//! `members.jsonl`/`merge_log.jsonl` idioms (`crate::app::registry::store`):
//! each line is one [`PeerEnvelope`]. It is the durable, reconstructable QUEUE the
//! broker fsyncs an envelope into BEFORE delivery and from which the restore-drain
//! redelivers — an envelope is removed ([`Mailbox::advance`]) ONLY AFTER the
//! receiver durably folds it (its stratum-1 World log records the envelope id),
//! never up-front, so a crash mid-drain cannot lose the undelivered remainder. The
//! dedup authority is the RECEIVER's World fold, NOT a broker-side ledger: there is
//! deliberately NO pre-apply "delivered" record here that could suppress a
//! redelivery the effectively-once guarantee depends on. See
//! docs/agent/world/ecs-runtime.md §"Durable peer delivery".

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::world::inputs::DurableEnvelope;
use crate::agent::world::surface::PeerEnvelopeId;
use crate::app::peer::PeerId;
use crate::error::Error;

/// The filename, inside an App's directory, of its peer-message inbox. Sits
/// alongside `metadata.json`, `resume.json`, and the membership logs.
pub const INBOX_FILE: &str = "inbox.jsonl";

/// One opaque peer-message envelope: who sent it and the payload, as forwarded
/// by the broker. The payload is a `serde_json::Value` so the broker stays a
/// switch — it relays the envelope without interpreting its contents. Mirrors the
/// `PeerSend`/`PeerDeliver` protocol frames (`crate::protocol`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerEnvelope {
    /// The sender's node-addressable identity.
    pub from: PeerId,
    /// The opaque message payload, relayed verbatim.
    pub payload: serde_json::Value,
}

impl PeerEnvelope {
    /// The [`DurableEnvelope`] (id + payload + auth) carried in this envelope's
    /// payload, when the payload is one. `None` for a legacy/plain payload that
    /// predates the durable protocol — such an envelope is delivered best-effort
    /// (undeduped). See docs/agent/world/ecs-runtime.md (§"Durable peer delivery").
    pub fn durable(&self) -> Option<DurableEnvelope> {
        serde_json::from_value::<DurableEnvelope>(self.payload.clone()).ok()
    }
}

/// The append-only inbox for one App, rooted at its on-disk directory. Holds the
/// peer-message envelopes addressed to the App while its worker was not running,
/// drained in arrival order when the App is restored.
pub struct Mailbox {
    /// The durable inbox queue (`inbox.jsonl`): the reconstructable outbox of
    /// envelopes awaiting the App's restore. Redelivered in arrival order on
    /// restore; an envelope is removed only AFTER the receiver durably folds it
    /// ([`Mailbox::advance`]), never up-front.
    path: PathBuf,
}

impl Mailbox {
    /// The inbox rooted at `app_dir`, the App's on-disk home directory (from
    /// `AppStore::app_dir`).
    pub fn in_dir(app_dir: &Path) -> Self {
        Self {
            path: app_dir.join(INBOX_FILE),
        }
    }

    /// The path of the inbox file, exposed for tests and diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one envelope to the inbox. Called by the broker when a message is
    /// addressed to an App whose worker is not running. Creates the file and any
    /// missing parent directories on first append, mirroring the JSONL append
    /// idiom in `crate::app::registry::store`. The write is `fsync`'d so an
    /// accepted-but-undelivered peer drive survives a crash right after acceptance
    /// — the durable outbox the queued set is reconstructable from (durable peer
    /// delivery, point 1). See docs/agent/world/ecs-runtime.md.
    pub fn enqueue(&self, envelope: &PeerEnvelope) -> Result<(), Error> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let mut line = serde_json::to_string(envelope).map_err(Error::Json)?;
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(Error::Io)?;
        std::io::Write::write_all(&mut file, line.as_bytes()).map_err(Error::Io)?;
        // fsync: the durable outbox must survive a crash immediately after an
        // accepted send, so an undelivered peer drive is never lost.
        file.sync_all().map_err(Error::Io)
    }

    /// `fsync` `envelope` into the durable inbox queue IDEMPOTENTLY by `id`: a
    /// no-op (returns `false`) if an envelope with the same durable id is already
    /// queued — it is already durable from a prior attempt. The durable relay
    /// fsyncs-before-deliver on EVERY attempt (INV-3), so a retry of the same
    /// logical send must NOT pile up duplicate inbox entries that a single
    /// per-envelope advance would then leave behind; a distinct id is always
    /// appended (returns `true`). See docs/agent/world/ecs-runtime.md.
    pub fn enqueue_unique(
        &self,
        envelope: &PeerEnvelope,
        id: &PeerEnvelopeId,
    ) -> Result<bool, Error> {
        let already = self
            .peek()?
            .into_iter()
            .any(|e| e.durable().map(|d| d.id).as_ref() == Some(id));
        if already {
            return Ok(false);
        }
        self.enqueue(envelope)?;
        Ok(true)
    }

    /// Whether the inbox currently holds any undelivered envelopes.
    pub fn is_empty(&self) -> bool {
        match fs::metadata(&self.path) {
            Ok(meta) => meta.len() == 0,
            Err(_) => true,
        }
    }

    /// The number of envelopes currently queued in the inbox. `0` when the
    /// inbox does not exist or is empty. Used by the broker's RejectNewest cap
    /// check (see docs/agent/world/ecs-runtime.md §"Durable peer delivery"
    /// §1335–1338): a full inbox rejects the incoming send without enqueueing.
    pub fn depth(&self) -> Result<usize, Error> {
        Ok(self.peek()?.len())
    }

    /// Read the queued envelopes without removing them. A missing inbox yields an
    /// empty list. A malformed line is skipped (logged) rather than failing the
    /// whole read, so one corrupt envelope never strands the rest.
    pub fn peek(&self) -> Result<Vec<PeerEnvelope>, Error> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(e)),
        };
        let mut envelopes = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<PeerEnvelope>(line) {
                Ok(envelope) => envelopes.push(envelope),
                Err(e) => log::warn!(
                    "skipping malformed inbox line in {}: {e}",
                    self.path.display()
                ),
            }
        }
        Ok(envelopes)
    }

    /// Drain the inbox: return every queued envelope in arrival order and clear
    /// the log so a delivered message is not re-delivered. Called on restore to
    /// hand the App's worker the messages addressed to it while it was offline. A
    /// missing inbox drains to an empty list.
    pub fn drain(&self) -> Result<Vec<PeerEnvelope>, Error> {
        let envelopes = self.peek()?;
        if envelopes.is_empty() {
            // Nothing queued: leave the filesystem untouched.
            return Ok(envelopes);
        }
        // Truncate by removing the file; the next `enqueue` re-creates it. This
        // keeps drain atomic-enough for the single-host broker: the file is read
        // fully before it is removed.
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
        Ok(envelopes)
    }

    /// ADVANCE the durable inbox past one envelope: remove the FIRST queued entry
    /// whose [`DurableEnvelope`] id is `id`, rewriting the remainder (`fsync`'d).
    /// Returns whether a matching envelope was removed.
    ///
    /// This is the crash-safe per-envelope advance (INV-4): the broker/restore-drain
    /// calls it ONLY AFTER the receiver durably folds the envelope (its stratum-1
    /// World log records the id). Because the removal happens strictly AFTER the
    /// durable fold, a crash in the window between fold and advance leaves the
    /// envelope queued — it is redelivered on the next restore and DEDUPED at the
    /// receiver's stratum-1 log (a NO-OP), never lost and never double-applied. The
    /// inbox is never truncated up-front, so the undelivered remainder always
    /// survives. See docs/agent/world/ecs-runtime.md §"Durable peer delivery".
    pub fn advance(&self, id: &PeerEnvelopeId) -> Result<bool, Error> {
        let queued = self.peek()?;
        let mut removed = false;
        let mut kept: Vec<PeerEnvelope> = Vec::with_capacity(queued.len());
        for envelope in queued {
            if !removed && envelope.durable().map(|d| d.id).as_ref() == Some(id) {
                removed = true; // drop exactly the first matching entry
            } else {
                kept.push(envelope);
            }
        }
        if removed {
            self.rewrite(&kept)?;
        }
        Ok(removed)
    }

    /// Remove and return the HEAD envelope of the durable inbox, rewriting the
    /// remainder (`fsync`'d); `None` when the inbox is empty. The arrival-order
    /// advance for a legacy/plain queued envelope that carries no durable id (so
    /// [`Mailbox::advance`] cannot key on one). Like `advance`, it is called only
    /// after the head has been delivered, so the remainder is never lost up-front.
    pub fn remove_first(&self) -> Result<Option<PeerEnvelope>, Error> {
        let mut queued = self.peek()?;
        if queued.is_empty() {
            return Ok(None);
        }
        let head = queued.remove(0);
        self.rewrite(&queued)?;
        Ok(Some(head))
    }

    /// Rewrite the inbox to exactly `envelopes` (`fsync`'d). An empty result removes
    /// the file so a later `enqueue` re-creates a fresh log, mirroring `drain`'s
    /// truncate. The whole-file rewrite is the single-host broker's atomic-enough
    /// advance: the inbox is read fully (`peek`) before it is rewritten.
    fn rewrite(&self, envelopes: &[PeerEnvelope]) -> Result<(), Error> {
        if envelopes.is_empty() {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Io(e)),
            }
            return Ok(());
        }
        let mut body = String::new();
        for envelope in envelopes {
            body.push_str(&serde_json::to_string(envelope).map_err(Error::Json)?);
            body.push('\n');
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .map_err(Error::Io)?;
        std::io::Write::write_all(&mut file, body.as_bytes()).map_err(Error::Io)?;
        // fsync: the advance must be durable so a crash right after it never
        // resurrects an already-folded envelope.
        file.sync_all().map_err(Error::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppId;

    fn envelope(from: &str, text: &str) -> PeerEnvelope {
        PeerEnvelope {
            from: PeerId::local(AppId(from.into())),
            payload: serde_json::json!({ "text": text }),
        }
    }

    #[test]
    fn enqueue_then_drain_returns_in_arrival_order() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());

        mailbox.enqueue(&envelope("a", "first")).unwrap();
        mailbox.enqueue(&envelope("b", "second")).unwrap();

        let drained = mailbox.drain().unwrap();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].payload["text"], "first");
        assert_eq!(drained[1].payload["text"], "second");
        assert_eq!(drained[0].from, PeerId::local(AppId("a".into())));
    }

    #[test]
    fn drain_clears_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&envelope("a", "hi")).unwrap();
        assert!(!mailbox.is_empty());

        let first = mailbox.drain().unwrap();
        assert_eq!(first.len(), 1);

        // A second drain after the first yields nothing — the message is not
        // re-delivered.
        let second = mailbox.drain().unwrap();
        assert!(second.is_empty());
        assert!(mailbox.is_empty());
    }

    #[test]
    fn peek_does_not_consume() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&envelope("a", "stay")).unwrap();

        assert_eq!(mailbox.peek().unwrap().len(), 1);
        // Peeking again still sees it.
        assert_eq!(mailbox.peek().unwrap().len(), 1);
        // And a drain still finds it.
        assert_eq!(mailbox.drain().unwrap().len(), 1);
    }

    #[test]
    fn missing_inbox_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        assert!(mailbox.is_empty());
        assert!(mailbox.peek().unwrap().is_empty());
        assert!(mailbox.drain().unwrap().is_empty());
    }

    #[test]
    fn enqueue_after_drain_starts_a_fresh_log() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&envelope("a", "one")).unwrap();
        mailbox.drain().unwrap();

        // Re-enqueue re-creates the log; only the new message is present.
        mailbox.enqueue(&envelope("b", "two")).unwrap();
        let drained = mailbox.drain().unwrap();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].payload["text"], "two");
    }

    // -- durable inbox queue: crash-safe per-envelope advance (INV-4) ----------

    /// A durable envelope (id + payload + auth) carrying envelope id `id`.
    fn durable(id: &str) -> crate::agent::world::inputs::DurableEnvelope {
        use crate::agent::world::inputs::{Authorization, DurableEnvelope, PeerPayload};
        use crate::agent::world::world::PeerId as WorldPeerId;
        DurableEnvelope {
            id: PeerEnvelopeId(id.into()),
            payload: PeerPayload::Message(serde_json::json!({ "text": "x" })),
            auth: Authorization {
                from: WorldPeerId { app_id: "a".into(), node_id: "local".into() },
                token: "a".into(),
            },
        }
    }

    /// A queued [`PeerEnvelope`] whose payload IS the durable envelope `id` — the
    /// shape the broker fsyncs into the inbox queue on the durable path.
    fn queued(id: &str) -> PeerEnvelope {
        PeerEnvelope {
            from: PeerId::local(AppId("a".into())),
            payload: serde_json::to_value(durable(id)).unwrap(),
        }
    }

    /// `advance(id)` removes EXACTLY the first queued envelope with that durable id,
    /// `fsync`'d, leaving the rest in arrival order — the per-envelope advance the
    /// drain performs ONLY after the receiver durably folds the head (INV-4).
    #[test]
    fn advance_removes_one_envelope_by_id_and_keeps_the_remainder() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&queued("env-1")).unwrap();
        mailbox.enqueue(&queued("env-2")).unwrap();

        // Advancing past env-1 leaves env-2 still queued.
        assert!(mailbox.advance(&PeerEnvelopeId("env-1".into())).unwrap());
        let rest = mailbox.peek().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].durable().map(|d| d.id), Some(PeerEnvelopeId("env-2".into())));

        // Advancing past a not-queued id removes nothing.
        assert!(!mailbox.advance(&PeerEnvelopeId("missing".into())).unwrap());
        assert_eq!(mailbox.peek().unwrap().len(), 1);

        // Advancing past env-2 empties the inbox.
        assert!(mailbox.advance(&PeerEnvelopeId("env-2".into())).unwrap());
        assert!(mailbox.is_empty());
    }

    /// A crash-window scenario at the queue level: an envelope fsynced into the
    /// inbox BEFORE delivery is NOT removed up-front — it survives until the
    /// receiver confirms a durable fold, so a crash before the advance leaves it
    /// redeliverable. Re-reading the inbox after the "crash" still finds it.
    #[test]
    fn enqueued_envelope_survives_until_advanced() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&queued("env-9")).unwrap();

        // "Crash" before any advance: a fresh Mailbox over the same dir still sees
        // the envelope — it was never dropped or deduped away.
        let reopened = Mailbox::in_dir(dir.path());
        let surviving = reopened.peek().unwrap();
        assert_eq!(surviving.len(), 1, "the un-advanced envelope persists across a crash");
        assert_eq!(surviving[0].durable().map(|d| d.id), Some(PeerEnvelopeId("env-9".into())));

        // Only after a durable-fold confirmation does the advance remove it.
        assert!(reopened.advance(&PeerEnvelopeId("env-9".into())).unwrap());
        assert!(reopened.is_empty());
    }

    /// `remove_first` pops the arrival-order head (`fsync`'d), the advance for a
    /// legacy/plain queued envelope that carries no durable id.
    #[test]
    fn remove_first_pops_the_head() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&envelope("a", "first")).unwrap();
        mailbox.enqueue(&envelope("b", "second")).unwrap();

        let head = mailbox.remove_first().unwrap().expect("a head");
        assert_eq!(head.payload["text"], "first");
        let rest = mailbox.peek().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].payload["text"], "second");

        assert!(mailbox.remove_first().unwrap().is_some());
        assert!(mailbox.remove_first().unwrap().is_none(), "an empty inbox pops None");
    }

    #[test]
    fn durable_extracts_the_full_envelope() {
        let envelope = PeerEnvelope {
            from: PeerId::local(AppId("a".into())),
            payload: serde_json::to_value(durable("env-1")).unwrap(),
        };
        assert_eq!(envelope.durable().map(|d| d.id), Some(PeerEnvelopeId("env-1".into())));

        // A plain (legacy) payload carries no durable envelope.
        let plain = PeerEnvelope {
            from: PeerId::local(AppId("a".into())),
            payload: serde_json::json!({ "x": 1 }),
        };
        assert!(plain.durable().is_none());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = Mailbox::in_dir(dir.path());
        mailbox.enqueue(&envelope("a", "good")).unwrap();
        // Append a corrupt line directly.
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(mailbox.path())
            .unwrap();
        std::io::Write::write_all(&mut f, b"{ not json\n").unwrap();

        let drained = mailbox.drain().unwrap();
        assert_eq!(drained.len(), 1, "the one good envelope survives a corrupt line");
        assert_eq!(drained[0].payload["text"], "good");
    }
}
