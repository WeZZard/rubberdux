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
//! each line is one [`PeerEnvelope`]. Draining reads the file, hands back the
//! envelopes, and truncates the log so a delivered message is not re-delivered.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// The append-only inbox for one App, rooted at its on-disk directory. Holds the
/// peer-message envelopes addressed to the App while its worker was not running,
/// drained in arrival order when the App is restored.
pub struct Mailbox {
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
    /// idiom in `crate::app::registry::store`.
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
        std::io::Write::write_all(&mut file, line.as_bytes()).map_err(Error::Io)
    }

    /// Whether the inbox currently holds any undelivered envelopes.
    pub fn is_empty(&self) -> bool {
        match fs::metadata(&self.path) {
            Ok(meta) => meta.len() == 0,
            Err(_) => true,
        }
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
