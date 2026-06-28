//! event_log — see docs/agent/world/ecs-runtime.md
//!
//! The event log is the runtime's **single source of truth** (Invariant 9).
//!
//! The imperative shell obeys the **log-before-apply discipline** (Invariant 4):
//! it appends the causing `Event` to the log BEFORE passing it to the reducer,
//! so that a crash between append and apply is recoverable — on resume the log
//! already holds the event and the reducer simply re-applies it, reconstructing
//! an identical `World` without re-invoking any external effect.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::agent::world::inputs::Event;
use crate::agent::world::lifecycle::LifecycleEvent;
use crate::error::Error;

// ---------------------------------------------------------------------------
// EventLog — append-only event log contract
// ---------------------------------------------------------------------------

/// An append-only log over TWO strata.
///
/// Stratum 1 ([`Event`]) is the replay-defining logical log: the imperative
/// shell calls [`EventLog::append`] with the causing `Event` BEFORE passing it
/// to the reducer (log-before-apply, Inv 4), and [`EventLog::load`] returns the
/// full ordered sequence so the replay driver can fold it into a reconstructed
/// `World`.
///
/// Stratum 2 ([`LifecycleEvent`]) is an OPERATIONAL trace stream kept ALONGSIDE
/// stratum 1 but never folded into the World on replay. The live driver calls
/// [`EventLog::append_lifecycle`] to write-ahead a `CommandDispatched`
/// dispatch-intent before an effect; [`EventLog::load_lifecycle`] is read ONLY
/// by the resume/recovery path. Because `load` returns stratum 1 alone, the
/// replay fold is structurally blind to stratum 2 (the two-strata convention).
/// See docs/agent/world/ecs-runtime.md.
pub trait EventLog: Send + Sync {
    /// Append one stratum-1 event to the log.
    ///
    /// The caller MUST invoke this before applying the event to the `World`
    /// (log-before-apply discipline, Inv 4).
    fn append(&mut self, event: &Event) -> Result<(), Error>;

    /// Load all stratum-1 events in append order.
    ///
    /// An empty log returns an empty `Vec` (genesis state, Inv 9). Stratum-2
    /// records are NOT returned here — replay folds stratum 1 only.
    fn load(&self) -> Result<Vec<Event>, Error>;

    /// Append one stratum-2 [`LifecycleEvent`] to the operational stream.
    ///
    /// The live driver calls this to write-ahead a `CommandDispatched`
    /// dispatch-intent BEFORE performing the effect (Inv 5). It is neutral on
    /// replay; only the resume path reads it back.
    fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error>;

    /// Load all stratum-2 [`LifecycleEvent`]s in append order.
    ///
    /// Read by the resume/recovery path, never by the replay fold. An empty
    /// stream returns an empty `Vec`.
    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error>;
}

// ---------------------------------------------------------------------------
// JSONL helpers — one serialized value per line, append-only
// ---------------------------------------------------------------------------

/// Append one value as a single JSON line to `path`, creating the file and any
/// missing parent directories. No in-memory buffer, so a returned `Ok(())` is
/// durable. Shared by both strata so the on-disk encoding is identical, and by
/// the segmented log (`segment.rs`) so a segment file is byte-identical to a
/// single-file log.
pub(crate) fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let json = serde_json::to_string(value)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Load every value from a JSONL file at `path`, in line order, skipping blank
/// lines. A missing file is genesis (empty `Vec`). `label` names the stream in
/// parse errors so a malformed line is attributable to its stratum. Shared with
/// the segmented log (`segment.rs`) so a segment loads with identical encoding.
pub(crate) fn load_jsonl<T: DeserializeOwned>(path: &Path, label: &str) -> Result<Vec<T>, Error> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };

    let reader = BufReader::new(file);
    let mut values = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: T = serde_json::from_str(&line)
            .map_err(|e| Error::World(format!("{label} line {}: {e}", idx + 1)))?;
        values.push(value);
    }
    Ok(values)
}

// ---------------------------------------------------------------------------
// FilesystemEventLog — one JSON object per line, append-only
// ---------------------------------------------------------------------------

/// A JSONL-backed append-only event log stored at `path`, with the stratum-2
/// operational stream in a sibling file.
///
/// Each [`FilesystemEventLog::append`] call opens the file in append mode and
/// writes exactly one `Event` JSON object followed by a newline, then closes it.
/// There is no in-memory write buffer, so any event that returns `Ok(())` is
/// durable before the reducer runs.  [`FilesystemEventLog::load`] reads
/// line-by-line and skips blank lines so the file is valid across multiple
/// runs and partial writes are detectable.  Stratum-2 [`LifecycleEvent`]s go to
/// a PARALLEL sibling file (`<name>.lifecycle.jsonl`) so the stratum-1 file the
/// replay fold reads stays byte-identical and free of operational records.
/// See docs/agent/world/ecs-runtime.md.
pub struct FilesystemEventLog {
    path: PathBuf,
}

impl FilesystemEventLog {
    /// Create a `FilesystemEventLog` backed by `path`.
    ///
    /// The file and any missing parent directories are created on the first
    /// `append` call.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The sibling path holding the stratum-2 operational stream, derived from
    /// the stratum-1 `path` so the two travel together without a second
    /// constructor argument.
    fn lifecycle_path(&self) -> PathBuf {
        self.path.with_extension("lifecycle.jsonl")
    }
}

impl EventLog for FilesystemEventLog {
    fn append(&mut self, event: &Event) -> Result<(), Error> {
        append_jsonl(&self.path, event)
    }

    fn load(&self) -> Result<Vec<Event>, Error> {
        load_jsonl(&self.path, "event log")
    }

    fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error> {
        append_jsonl(&self.lifecycle_path(), event)
    }

    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
        load_jsonl(&self.lifecycle_path(), "lifecycle log")
    }
}

// ---------------------------------------------------------------------------
// MemoryEventLog — Vec-backed, for unit tests
// ---------------------------------------------------------------------------

/// A Vec-backed in-memory event log over both strata.
///
/// Not durable — intended for unit tests where `FilesystemEventLog`'s
/// filesystem interaction is undesired. Stratum 1 and stratum 2 are held in
/// SEPARATE vectors, mirroring the filesystem log's parallel-stream layout, so
/// `load` (stratum 1) never observes a `LifecycleEvent`.
#[derive(Default)]
pub struct MemoryEventLog {
    events: Vec<Event>,
    lifecycle: Vec<LifecycleEvent>,
}

impl MemoryEventLog {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventLog for MemoryEventLog {
    fn append(&mut self, event: &Event) -> Result<(), Error> {
        self.events.push(event.clone());
        Ok(())
    }

    fn load(&self) -> Result<Vec<Event>, Error> {
        Ok(self.events.clone())
    }

    fn append_lifecycle(&mut self, event: &LifecycleEvent) -> Result<(), Error> {
        self.lifecycle.push(event.clone());
        Ok(())
    }

    fn load_lifecycle(&self) -> Result<Vec<LifecycleEvent>, Error> {
        Ok(self.lifecycle.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::inputs::{Fingerprint, LogicalInput, Origin};
    use crate::agent::world::lifecycle::{
        ActorCtx, AppId, EffectKind, IdempotencyKey, LifecycleEvent,
    };

    /// A stratum-2 dispatch-intent fixture — the write-ahead record the live
    /// driver appends before an effect.
    fn sample_dispatch(cmd: u32, effect_id: u32) -> LifecycleEvent {
        LifecycleEvent::CommandDispatched {
            at: 2,
            cmd,
            kind: EffectKind::CallModel,
            ctx: ActorCtx {
                entity: 0,
                origin: Origin::Agent,
                edge: 0,
            },
            key: IdempotencyKey {
                app_id: AppId::default(),
                tick: 2,
                effect_id,
            },
            fingerprint: Fingerprint("fp-req-1".into()),
        }
    }

    fn sample_events() -> Vec<Event> {
        vec![
            Event {
                origin: Origin::System,
                edge: 0,
                at: 0,
                wall: Some(1_700_000_000),
                input: LogicalInput::SessionStarted {
                    seed: 0xDEAD_BEEF,
                    surface_tools: Vec::new(),
                },
            },
            Event {
                origin: Origin::Human,
                edge: 0,
                at: 1,
                wall: None,
                input: LogicalInput::UserMessage {
                    to: 0,
                    text: "hello".into(),
                },
            },
            Event {
                origin: Origin::Human,
                edge: 1,
                at: 2,
                wall: Some(1_700_000_002),
                input: LogicalInput::UserMessage {
                    to: 1,
                    text: "world".into(),
                },
            },
        ]
    }

    // -----------------------------------------------------------------------
    // MemoryEventLog
    // -----------------------------------------------------------------------

    #[test]
    fn memory_log_genesis_is_empty() {
        let log = MemoryEventLog::new();
        let loaded = log.load().expect("load empty memory log");
        assert!(loaded.is_empty(), "genesis state must be an empty Vec");
    }

    #[test]
    fn memory_log_append_and_reload_roundtrip() {
        let events = sample_events();
        let mut log = MemoryEventLog::new();
        for e in &events {
            log.append(e).expect("append to memory log");
        }
        let loaded = log.load().expect("load memory log");
        assert_eq!(loaded, events, "reloaded sequence must equal appended sequence");
    }

    #[test]
    fn memory_log_lifecycle_stream_is_parallel_and_neutral_to_stratum_one() {
        let events = sample_events();
        let mut log = MemoryEventLog::new();
        for e in &events {
            log.append(e).expect("append stratum-1");
        }
        // Interleave stratum-2 dispatch-intents among the stratum-1 appends.
        let dispatches = vec![sample_dispatch(0, 0), sample_dispatch(1, 1)];
        for d in &dispatches {
            log.append_lifecycle(d).expect("append stratum-2");
        }

        // Stratum-1 `load` is UNAFFECTED by the stratum-2 appends (the replay fold
        // is structurally blind to operational records).
        assert_eq!(
            log.load().expect("load stratum-1"),
            events,
            "stratum-2 records must not appear in the stratum-1 load"
        );
        // Stratum-2 is retrievable on its own stream (resume reads it).
        assert_eq!(
            log.load_lifecycle().expect("load stratum-2"),
            dispatches,
            "stratum-2 stream must round-trip its records in order"
        );
    }

    // -----------------------------------------------------------------------
    // FilesystemEventLog
    // -----------------------------------------------------------------------

    #[test]
    fn filesystem_log_genesis_is_empty() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("events.jsonl");
        let log = FilesystemEventLog::new(&path);
        let loaded = log.load().expect("load missing-file log");
        assert!(loaded.is_empty(), "missing file must yield empty Vec");
    }

    #[test]
    fn filesystem_log_append_and_reload_roundtrip() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("events.jsonl");
        let events = sample_events();
        let mut log = FilesystemEventLog::new(&path);
        for e in &events {
            log.append(e).expect("append to filesystem log");
        }
        let loaded = log.load().expect("load filesystem log");
        assert_eq!(
            loaded, events,
            "reloaded sequence must equal appended sequence byte-for-byte"
        );
    }

    #[test]
    fn filesystem_log_lifecycle_stream_lives_in_a_sibling_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("events.jsonl");
        let events = sample_events();
        let dispatches = vec![sample_dispatch(0, 0), sample_dispatch(1, 1)];

        let mut log = FilesystemEventLog::new(&path);
        for e in &events {
            log.append(e).expect("append stratum-1");
        }
        for d in &dispatches {
            log.append_lifecycle(d).expect("append stratum-2");
        }

        // Stratum 2 lands in a parallel sibling file, leaving the stratum-1 file
        // byte-identical and operational-record-free.
        assert!(
            dir.path().join("events.lifecycle.jsonl").exists(),
            "stratum-2 must be written to the sibling lifecycle file"
        );
        assert_eq!(
            log.load().expect("load stratum-1"),
            events,
            "stratum-1 file must not contain stratum-2 records"
        );
        assert_eq!(
            log.load_lifecycle().expect("load stratum-2"),
            dispatches,
            "stratum-2 sibling file must round-trip its records in order"
        );

        // A fresh handle over the same path reads both streams back (durable).
        let reopened = FilesystemEventLog::new(&path);
        assert_eq!(reopened.load_lifecycle().expect("reload stratum-2"), dispatches);
    }
}
