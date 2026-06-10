//! The App worker lifecycle state machine and the idle sweeper.
//!
//! An App's worker is either `Active` (its subprocess is running) or
//! `Tombstoned` (its subprocess has exited and its resumable state is persisted
//! to disk). Tombstoning is an idle-eviction optimization: an App that has been
//! quiet long enough has its worker suspended to free host resources, and the
//! next message transparently restores it — the caller never observes the gap.
//! The state-machine transitions and the durable [`ResumeState`] live here; the
//! [`LocalSupervisor`](crate::app::runtime::local_supervisor::LocalSupervisor)
//! drives them. The rationale is in `docs/app/runtime/worker-lifecycle.md`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent::interaction::AgentInteraction;
use crate::error::Error;

/// The filename, inside an App's directory, of the durable resume record an
/// `Active` worker leaves behind when it is tombstoned. The App's continuously
/// persisted `session.jsonl` carries the conversation history; `resume.json`
/// carries only the transient state that would otherwise be lost when the
/// subprocess exits.
pub const RESUME_FILE: &str = "resume.json";

/// The default idle window, in seconds, before an `Active` App with no pending
/// interaction or in-flight turn is tombstoned. Overridable via
/// [`RUBBERDUX_APP_IDLE_SECS_ENV`].
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// Environment variable that overrides [`DEFAULT_IDLE_SECS`]. Named after what
/// the value represents (the App idle window), not after any timer library.
pub const RUBBERDUX_APP_IDLE_SECS_ENV: &str = "RUBBERDUX_APP_IDLE_SECS";

/// Resolve the configured idle window from the environment, falling back to
/// [`DEFAULT_IDLE_SECS`] when the variable is unset or unparsable.
pub fn idle_window() -> Duration {
    let secs = std::env::var(RUBBERDUX_APP_IDLE_SECS_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_SECS);
    Duration::from_secs(secs)
}

/// The lifecycle phase of an App's worker. Mirrors
/// [`crate::app::AppStatus`] but is the *runtime* truth the supervisor owns,
/// distinct from the on-disk manifest (which always loads `Tombstoned`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecyclePhase {
    /// The worker subprocess is running.
    Active,
    /// The worker subprocess has exited; its resumable state is on disk.
    Tombstoned,
}

/// The runtime activity record the idle sweeper reads to decide whether an
/// `Active` App may be tombstoned. It tracks the lifecycle phase, the last time
/// the App did anything observable, whether a turn is in flight, and whether any
/// interaction is awaiting a user answer. An App is *idle-evictable* only when it
/// is `Active`, has no in-flight turn, has no pending interaction, and has been
/// quiet longer than the idle window.
///
/// Immutable transitions: each mutator returns a new value rather than mutating
/// in place, matching the project's functional-over-imperative posture.
#[derive(Debug, Clone)]
pub struct AppLifecycle {
    phase: LifecyclePhase,
    /// When the App last did anything observable (a message, a turn boundary, a
    /// restore). Compared against the idle window by [`Self::is_idle_evictable`].
    last_activity: Instant,
    /// `true` between the start of a turn and its final entry; an in-flight turn
    /// blocks tombstoning so a working App is never evicted mid-thought.
    turn_in_flight: bool,
    /// `true` while at least one interaction awaits a user answer; a blocked App
    /// is not idle and must not be tombstoned.
    has_pending_interaction: bool,
}

impl AppLifecycle {
    /// A freshly `Active` App, last-active as of `now`, with no in-flight turn
    /// and no pending interaction.
    pub fn active(now: Instant) -> Self {
        Self {
            phase: LifecyclePhase::Active,
            last_activity: now,
            turn_in_flight: false,
            has_pending_interaction: false,
        }
    }

    /// The current lifecycle phase.
    pub fn phase(&self) -> LifecyclePhase {
        self.phase
    }

    /// Record observable activity at `now`, returning the updated record. Bumps
    /// the idle clock so the App is not a tombstoning candidate until the window
    /// elapses again from this moment.
    pub fn touched(self, now: Instant) -> Self {
        Self {
            last_activity: now,
            ..self
        }
    }

    /// Mark a turn as started: the App is busy until [`Self::turn_finished`].
    /// Also counts as activity, so the idle clock is reset.
    pub fn turn_started(self, now: Instant) -> Self {
        Self {
            last_activity: now,
            turn_in_flight: true,
            ..self
        }
    }

    /// Mark the in-flight turn as finished, refreshing the idle clock so the
    /// window is measured from the end of the work.
    pub fn turn_finished(self, now: Instant) -> Self {
        Self {
            last_activity: now,
            turn_in_flight: false,
            ..self
        }
    }

    /// Record whether any interaction is awaiting a user answer. A blocked App is
    /// not idle-evictable regardless of how long it has been quiet.
    pub fn with_pending_interaction(self, has_pending: bool) -> Self {
        Self {
            has_pending_interaction: has_pending,
            ..self
        }
    }

    /// Transition to `Tombstoned`: the worker subprocess has exited. Clears the
    /// in-flight-turn flag because a suspended App has no running turn.
    pub fn tombstoned(self) -> Self {
        Self {
            phase: LifecyclePhase::Tombstoned,
            turn_in_flight: false,
            ..self
        }
    }

    /// Transition back to `Active` at `now`: the worker has been restored. Resets
    /// the idle clock and clears the in-flight-turn flag.
    pub fn restored(self, now: Instant) -> Self {
        Self {
            phase: LifecyclePhase::Active,
            last_activity: now,
            turn_in_flight: false,
            ..self
        }
    }

    /// Whether the sweeper may tombstone this App as of `now`: it must be
    /// `Active`, have no in-flight turn, have no pending interaction, and have
    /// been quiet for at least `idle_window`.
    pub fn is_idle_evictable(&self, now: Instant, idle_window: Duration) -> bool {
        self.phase == LifecyclePhase::Active
            && !self.turn_in_flight
            && !self.has_pending_interaction
            && now.duration_since(self.last_activity) >= idle_window
    }
}

/// The durable state a tombstoned App leaves behind, persisted as `resume.json`
/// in the App's directory. The conversation history is *not* duplicated here —
/// it is continuously persisted to the App's `session.jsonl` by the worker's
/// store — so this record carries only what would otherwise be lost when the
/// subprocess exits: the interactions awaiting an answer and (for a later task)
/// the peer messages not yet delivered to the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResumeState {
    /// Interactions the worker had raised and was awaiting answers for at the
    /// moment it was tombstoned. Re-presented when the App is restored.
    #[serde(default)]
    pub pending_interactions: Vec<AgentInteraction>,
    /// A documented slot for peer messages that were addressed to this App but
    /// not yet delivered to its worker. Peer messaging is a later task, so this
    /// is empty for now; the field exists so the on-disk format is stable from
    /// the start. See `docs/app/runtime/worker-lifecycle.md`.
    #[serde(default)]
    pub undelivered_peer_messages: Vec<serde_json::Value>,
}

impl ResumeState {
    /// The path of the resume record inside `app_dir`.
    pub fn path_in(app_dir: &Path) -> PathBuf {
        app_dir.join(RESUME_FILE)
    }

    /// Persist this record to `resume.json` under `app_dir`, creating the
    /// directory if needed. Overwrites any prior record (last-writer-wins),
    /// mirroring the manifest writer in `crate::app::registry::store`.
    pub fn persist(&self, app_dir: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(app_dir).map_err(Error::Io)?;
        let json = serde_json::to_string_pretty(self).map_err(Error::Json)?;
        std::fs::write(Self::path_in(app_dir), json).map_err(Error::Io)
    }

    /// Load the resume record from `app_dir`. A missing or unreadable file yields
    /// a default (empty) record so a first restore — or a restore after a crash
    /// that left no record — proceeds with the durable history alone rather than
    /// failing.
    pub fn load(app_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path_in(app_dir)) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                log::warn!(
                    "ignoring malformed resume record at {}: {e}",
                    app_dir.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Remove the resume record from `app_dir`, if present. Called after a
    /// restore consumes it so a later tombstone writes a fresh record rather than
    /// leaving a stale one behind. A missing file is not an error.
    pub fn clear(app_dir: &Path) -> Result<(), Error> {
        match std::fs::remove_file(Self::path_in(app_dir)) {
            Ok(()) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::interaction::ApprovalFlavor;

    fn sample_interaction(request_id: &str) -> AgentInteraction {
        AgentInteraction::Approval {
            request_id: request_id.into(),
            app_id: "app".into(),
            flavor: ApprovalFlavor::Permission,
            prompt: "proceed?".into(),
        }
    }

    #[test]
    fn idle_window_defaults_without_env() {
        // The default applies when the variable is unset; we do not mutate the
        // process env here to stay parallel-safe, so assert the constant directly.
        assert_eq!(DEFAULT_IDLE_SECS, 300);
        // And the resolver never panics, whatever the ambient value is.
        let _ = idle_window();
    }

    #[test]
    fn new_active_app_is_not_immediately_evictable() {
        let now = Instant::now();
        let app = AppLifecycle::active(now);
        assert_eq!(app.phase(), LifecyclePhase::Active);
        assert!(!app.is_idle_evictable(now, Duration::from_secs(300)));
    }

    #[test]
    fn quiet_active_app_becomes_evictable_after_window() {
        let start = Instant::now();
        let app = AppLifecycle::active(start);
        let later = start + Duration::from_secs(301);
        assert!(app.is_idle_evictable(later, Duration::from_secs(300)));
    }

    #[test]
    fn activity_resets_the_idle_clock() {
        let start = Instant::now();
        let app = AppLifecycle::active(start);
        let mid = start + Duration::from_secs(200);
        let app = app.touched(mid);
        // 301s after start is only 101s after the touch: not yet evictable.
        let later = start + Duration::from_secs(301);
        assert!(!app.is_idle_evictable(later, Duration::from_secs(300)));
    }

    #[test]
    fn in_flight_turn_blocks_eviction() {
        let start = Instant::now();
        let app = AppLifecycle::active(start).turn_started(start);
        let later = start + Duration::from_secs(10_000);
        assert!(!app.is_idle_evictable(later, Duration::from_secs(300)));
        // Finishing the turn clears the block, but resets the idle clock.
        let app = app.turn_finished(later);
        assert!(!app.is_idle_evictable(later, Duration::from_secs(300)));
        let after = later + Duration::from_secs(301);
        assert!(app.is_idle_evictable(after, Duration::from_secs(300)));
    }

    #[test]
    fn pending_interaction_blocks_eviction() {
        let start = Instant::now();
        let app = AppLifecycle::active(start).with_pending_interaction(true);
        let later = start + Duration::from_secs(10_000);
        assert!(!app.is_idle_evictable(later, Duration::from_secs(300)));
        // Answering it clears the block.
        let app = app.with_pending_interaction(false);
        assert!(app.is_idle_evictable(later, Duration::from_secs(300)));
    }

    #[test]
    fn tombstoned_app_is_never_evictable() {
        let start = Instant::now();
        let app = AppLifecycle::active(start).tombstoned();
        assert_eq!(app.phase(), LifecyclePhase::Tombstoned);
        let later = start + Duration::from_secs(10_000);
        assert!(!app.is_idle_evictable(later, Duration::from_secs(300)));
    }

    #[test]
    fn tombstone_then_restore_round_trips_phase() {
        let start = Instant::now();
        let app = AppLifecycle::active(start).turn_started(start).tombstoned();
        assert_eq!(app.phase(), LifecyclePhase::Tombstoned);
        let resumed = start + Duration::from_secs(10_000);
        let app = app.restored(resumed);
        assert_eq!(app.phase(), LifecyclePhase::Active);
        // A just-restored App is not immediately evictable.
        assert!(!app.is_idle_evictable(resumed, Duration::from_secs(300)));
    }

    #[test]
    fn resume_state_persist_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = ResumeState {
            pending_interactions: vec![sample_interaction("r1"), sample_interaction("r2")],
            undelivered_peer_messages: Vec::new(),
        };
        state.persist(dir.path()).unwrap();
        assert!(ResumeState::path_in(dir.path()).is_file());

        let loaded = ResumeState::load(dir.path());
        assert_eq!(loaded, state);
        assert_eq!(loaded.pending_interactions.len(), 2);
        assert_eq!(loaded.pending_interactions[0].request_id(), "r1");
    }

    #[test]
    fn load_missing_resume_state_is_empty_default() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = ResumeState::load(dir.path());
        assert_eq!(loaded, ResumeState::default());
        assert!(loaded.pending_interactions.is_empty());
        assert!(loaded.undelivered_peer_messages.is_empty());
    }

    #[test]
    fn clear_removes_resume_state_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        ResumeState::default().persist(dir.path()).unwrap();
        assert!(ResumeState::path_in(dir.path()).is_file());
        ResumeState::clear(dir.path()).unwrap();
        assert!(!ResumeState::path_in(dir.path()).is_file());
        // A second clear on a missing file is not an error.
        ResumeState::clear(dir.path()).unwrap();
    }
}
