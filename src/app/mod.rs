//! The persistent `App` domain — the whiteboard's cluster entity.
//!
//! An App is a durable unit on the whiteboard (see `docs/apps/whiteboard.md`):
//! it *is* an agent that works while the user observes and approves. An App is a
//! cluster that aggregates member sessions and carries its own identity (title +
//! icon), a spatial home on the board, a topic summary, and the bookkeeping the
//! supervisor needs to tombstone and restore it. The filesystem persistence for
//! this domain lives in `registry/`; the design rationale is in
//! `docs/app/whiteboard-backend.md`.

pub mod identity;
pub mod registry;
pub mod runtime;
pub mod supervisor;

use serde::{Deserialize, Serialize};

/// Stable identifier for an [`App`], formatted like a session id
/// (`YYYY-MM-DD-hh-mm-ss-UTC`) so an App's on-disk directory name is sortable by
/// creation time, mirroring `crate::session::SessionId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

impl AppId {
    /// Mint a fresh id from the current UTC instant.
    pub fn now() -> Self {
        Self(
            chrono::Utc::now()
                .format("%Y-%m-%d-%H-%M-%S-UTC")
                .to_string(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The icon shown for an App on the board: a system-native symbol glyph plus a
/// generated color (see `docs/apps/whiteboard.md` decision D3). AI-generated
/// illustrated icons are explicitly out of scope; this keeps icons instant,
/// free, and visually coherent across the whole board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IconSpec {
    /// The SF-Symbol name on Apple platforms (a system symbol elsewhere).
    pub symbol: String,
    /// The generated color, stored as an `#RRGGBB` hex string so it crosses the
    /// API boundary without committing to a platform color type.
    pub color: String,
}

/// Where an App sits on the dot-grid board, in board cell coordinates. Spatial
/// position carries memory, so it is part of the App's persisted identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardPosition {
    pub row: i32,
    pub column: i32,
}

/// Lifecycle state of an App's agent worker (see `docs/apps/whiteboard.md`
/// decision D4). On host startup every App loads `Tombstoned`: the supervisor
/// has not yet spawned any worker, so no App is `Active` until it is restored on
/// demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppStatus {
    /// The worker is running.
    Active,
    /// The worker's state is persisted and its subprocess is not running; it is
    /// restored on demand, appearing as though it never left.
    Tombstoned,
}

/// A persistent App: the whiteboard's cluster entity. It aggregates member
/// sessions and owns the identity, position, and topic the user sees, plus the
/// most-recently-used timestamp the board orders by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    pub id: AppId,
    /// Short descriptive title derived from the originating task.
    pub title: String,
    pub icon: IconSpec,
    pub position: BoardPosition,
    pub status: AppStatus,
    /// Sessions clustered into this App. Stored in the manifest for a quick
    /// snapshot; the append-only `members.jsonl` log is the source of truth for
    /// how the membership evolved.
    pub member_session_ids: Vec<String>,
    /// The App's topic, summarized from its member sessions.
    pub summary: String,
    /// When set, the user has pinned this App's membership so auto-clustering
    /// must not move its sessions.
    pub user_locked: bool,
    /// Most-recently-used timestamp (RFC 3339) for board ordering.
    pub last_active: String,
}

impl App {
    /// Create a new App at the given board position. New Apps start
    /// [`AppStatus::Tombstoned`] (no worker has been spawned yet) and `last_active`
    /// is the creation instant.
    pub fn new(id: AppId, title: String, icon: IconSpec, position: BoardPosition) -> Self {
        Self {
            id,
            title,
            icon,
            position,
            status: AppStatus::Tombstoned,
            member_session_ids: Vec::new(),
            summary: String::new(),
            user_locked: false,
            last_active: chrono::Utc::now().to_rfc3339(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_app() -> App {
        App::new(
            AppId("2026-06-10-00-00-00-UTC".into()),
            "Plan the trip".into(),
            IconSpec {
                symbol: "airplane".into(),
                color: "#3478F6".into(),
            },
            BoardPosition { row: 2, column: 5 },
        )
    }

    #[test]
    fn new_app_is_tombstoned() {
        assert_eq!(sample_app().status, AppStatus::Tombstoned);
    }

    #[test]
    fn app_serialization_roundtrip() {
        let app = sample_app();
        let json = serde_json::to_string(&app).unwrap();
        let restored: App = serde_json::from_str(&json).unwrap();
        assert_eq!(app, restored);
    }

    #[test]
    fn app_status_serializes_snake_case() {
        let json = serde_json::to_string(&AppStatus::Tombstoned).unwrap();
        assert_eq!(json, "\"tombstoned\"");
        let json = serde_json::to_string(&AppStatus::Active).unwrap();
        assert_eq!(json, "\"active\"");
    }

    #[test]
    fn app_id_now_is_sortable_format() {
        let id = AppId::now();
        assert!(id.as_str().ends_with("-UTC"));
        assert_eq!(id.as_str().len(), 23);
    }
}
