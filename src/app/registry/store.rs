//! The App registry store: the trait describing App persistence and a
//! filesystem implementation rooted at `~/.rubberdux/apps/`.
//!
//! Layout per App (see `docs/app/whiteboard-backend.md`):
//!
//! ```text
//! ~/.rubberdux/apps/{id}/metadata.json     -- the App manifest (last-writer-wins)
//! ~/.rubberdux/apps/{id}/members.jsonl      -- append-only membership log
//! ~/.rubberdux/apps/{id}/merge_log.jsonl    -- append-only cluster-merge log
//! ~/.rubberdux/apps/archives/{id}/          -- archived Apps moved out of `list`
//! ```
//!
//! `RUBBERDUX_HOME` resolution mirrors `crate::session::SessionManager`; the
//! JSONL append/read idioms mirror `crate::agent::runtime::history_store`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::{App, AppId, AppStatus};
use crate::error::Error;

/// One line of the append-only `members.jsonl` log: a record of a session being
/// clustered into (or out of) the App, kept so membership history survives even
/// though `metadata.json` only holds the current snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRecord {
    pub session_id: String,
    /// RFC 3339 timestamp of when the change was recorded.
    pub recorded_at: String,
    /// `true` when the session joined, `false` when it left.
    pub joined: bool,
}

/// One line of the append-only `merge_log.jsonl`: a record of another App's
/// sessions being merged into this App during auto-clustering. Implementing the
/// merge mechanism itself is out of scope here; the log shape is defined so the
/// store can own the file from the start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRecord {
    /// The App whose sessions were absorbed.
    pub source_app_id: String,
    pub recorded_at: String,
}

/// Persistence contract for the App domain. Apps are created, listed, fetched by
/// id, and archived; archiving removes an App from the default listing while
/// leaving it addressable elsewhere.
pub trait AppStore: Send + Sync {
    /// Persist a new App's directory and manifest. Errors if it already exists.
    fn create(&self, app: &App) -> Result<(), Error>;

    /// List Apps. When `include_archived` is false, archived Apps are omitted.
    /// Every returned App loads with [`AppStatus::Tombstoned`]: on host startup
    /// no worker is running yet.
    fn list(&self, include_archived: bool) -> Result<Vec<App>, Error>;

    /// Fetch a single App by id, searching the live directory then the archive.
    fn get(&self, id: &AppId) -> Result<Option<App>, Error>;

    /// The App's on-disk home directory: the root a native worker is told to
    /// place its session data under via `--app-session-dir`, so all of an App's
    /// data lives inside the App's own directory. See
    /// `docs/app/runtime/worker-lifecycle.md`.
    fn app_dir(&self, id: &AppId) -> PathBuf;

    /// Move an App's directory under the archives subdir, removing it from
    /// `list(include_archived: false)` while keeping it addressable via `get`.
    fn archive(&self, id: &AppId) -> Result<(), Error>;

    /// Append a membership change to the App's `members.jsonl` log.
    fn record_member(&self, id: &AppId, record: &MemberRecord) -> Result<(), Error>;

    /// Append a merge to the App's `merge_log.jsonl` log.
    fn record_merge(&self, id: &AppId, record: &MergeRecord) -> Result<(), Error>;
}

const METADATA_FILE: &str = "metadata.json";
const MEMBERS_FILE: &str = "members.jsonl";
const MERGE_LOG_FILE: &str = "merge_log.jsonl";
const ARCHIVES_DIR: &str = "archives";

/// Filesystem-backed [`AppStore`] rooted at `~/.rubberdux/apps/`.
pub struct FilesystemAppStore {
    apps_dir: PathBuf,
}

impl FilesystemAppStore {
    /// Construct a store, resolving `RUBBERDUX_HOME` (default `~/.rubberdux`) the
    /// same way `crate::session::SessionManager` does, then rooting at its
    /// `apps/` subdirectory.
    pub fn new() -> Self {
        Self {
            apps_dir: Self::resolve_home().join("apps"),
        }
    }

    /// Construct a store rooted directly at `apps_dir`. Primarily for tests that
    /// isolate a temp directory.
    pub fn with_apps_dir(apps_dir: PathBuf) -> Self {
        Self { apps_dir }
    }

    fn resolve_home() -> PathBuf {
        std::env::var("RUBBERDUX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".rubberdux")
            })
    }

    fn archive_dir(&self, id: &AppId) -> PathBuf {
        self.apps_dir.join(ARCHIVES_DIR).join(id.as_str())
    }

    /// Read and parse the manifest in a single App directory. Returns `Ok(None)`
    /// for a directory without a readable, well-formed manifest so a malformed
    /// `metadata.json` is skipped rather than failing the whole listing.
    fn read_manifest(dir: &Path) -> Option<App> {
        let raw = fs::read_to_string(dir.join(METADATA_FILE)).ok()?;
        match serde_json::from_str::<App>(&raw) {
            Ok(mut app) => {
                // Every App loads Tombstoned: no worker runs at startup.
                app.status = AppStatus::Tombstoned;
                Some(app)
            }
            Err(e) => {
                log::warn!("skipping malformed App manifest at {}: {e}", dir.display());
                None
            }
        }
    }

    /// List the Apps directly contained in `dir` (one level deep), skipping the
    /// `archives` subdirectory and any directory without a valid manifest.
    fn list_dir(dir: &Path) -> Result<Vec<App>, Error> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            // No apps directory yet means no apps — an empty list, not an error.
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(e)),
        };

        let mut apps = Vec::new();
        for entry in entries {
            let entry = entry.map_err(Error::Io)?;
            if !entry.file_type().map_err(Error::Io)?.is_dir() {
                continue;
            }
            if entry.file_name() == ARCHIVES_DIR {
                continue;
            }
            if let Some(app) = Self::read_manifest(&entry.path()) {
                apps.push(app);
            }
        }
        Ok(apps)
    }

    /// Create an empty file at `path` if it does not already exist, without
    /// truncating an existing one. Used at `create` time to materialize the
    /// append-only logs as part of the App-directory contract.
    fn touch(path: &Path) -> Result<(), Error> {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|_| ())
            .map_err(Error::Io)
    }

    fn write_manifest(dir: &Path, app: &App) -> Result<(), Error> {
        let json = serde_json::to_string_pretty(app).map_err(Error::Json)?;
        fs::write(dir.join(METADATA_FILE), json).map_err(Error::Io)
    }

    /// Append one JSON record as a line to `path`, creating parent dirs and the
    /// file as needed. Mirrors the JSONL append idiom in
    /// `crate::agent::runtime::history_store`.
    fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let mut line = serde_json::to_string(value).map_err(Error::Json)?;
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(Error::Io)?;
        std::io::Write::write_all(&mut file, line.as_bytes()).map_err(Error::Io)
    }
}

impl Default for FilesystemAppStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AppStore for FilesystemAppStore {
    fn create(&self, app: &App) -> Result<(), Error> {
        let dir = self.app_dir(&app.id);
        if dir.exists() {
            return Err(Error::App(format!("App `{}` already exists", app.id)));
        }
        fs::create_dir_all(&dir).map_err(Error::Io)?;
        Self::write_manifest(&dir, app)?;
        // The append-only logs are part of the App-directory contract, so they
        // exist (empty) from creation rather than appearing lazily on first
        // `record_member`/`record_merge`. This makes the directory layout the
        // same whether or not membership/merge events have happened yet.
        Self::touch(&dir.join(MEMBERS_FILE))?;
        Self::touch(&dir.join(MERGE_LOG_FILE))?;
        Ok(())
    }

    fn list(&self, include_archived: bool) -> Result<Vec<App>, Error> {
        let mut apps = Self::list_dir(&self.apps_dir)?;
        if include_archived {
            apps.extend(Self::list_dir(&self.apps_dir.join(ARCHIVES_DIR))?);
        }
        Ok(apps)
    }

    fn app_dir(&self, id: &AppId) -> PathBuf {
        self.apps_dir.join(id.as_str())
    }

    fn get(&self, id: &AppId) -> Result<Option<App>, Error> {
        let live = self.app_dir(id);
        if live.is_dir() {
            return Ok(Self::read_manifest(&live));
        }
        let archived = self.archive_dir(id);
        if archived.is_dir() {
            return Ok(Self::read_manifest(&archived));
        }
        Ok(None)
    }

    fn archive(&self, id: &AppId) -> Result<(), Error> {
        let src = self.app_dir(id);
        if !src.is_dir() {
            return Err(Error::App(format!("App `{id}` does not exist")));
        }
        let dst = self.archive_dir(id);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        fs::rename(&src, &dst).map_err(Error::Io)
    }

    fn record_member(&self, id: &AppId, record: &MemberRecord) -> Result<(), Error> {
        Self::append_jsonl(&self.app_dir(id).join(MEMBERS_FILE), record)
    }

    fn record_merge(&self, id: &AppId, record: &MergeRecord) -> Result<(), Error> {
        Self::append_jsonl(&self.app_dir(id).join(MERGE_LOG_FILE), record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{BoardPosition, IconSpec};

    /// Serializes every test that reads or writes the process-global
    /// `RUBBERDUX_HOME` env var. Without this, parallel tests racing on the same
    /// env var would see each other's values; the lock makes each set+restore
    /// atomic with respect to the store construction it governs.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_apps_dir() -> PathBuf {
        // Combine a process-wide atomic counter with the nanosecond clock so
        // concurrently running tests never collide on a directory name.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rubberdux-app-test-{ts}-{n}"))
    }

    fn sample_app(id: &str) -> App {
        App::new(
            AppId(id.into()),
            "Plan the trip".into(),
            IconSpec {
                symbol: "airplane".into(),
                color: "#3478F6".into(),
            },
            BoardPosition { row: 1, column: 1 },
        )
    }

    #[test]
    fn create_list_get_roundtrip() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());
        let app = sample_app("2026-06-10-00-00-00-UTC");

        store.create(&app).unwrap();

        // The append-only logs are materialized (empty) at create time as part
        // of the App-directory contract, not lazily on first append.
        let app_dir = dir.join(app.id.as_str());
        assert!(app_dir.join(MEMBERS_FILE).is_file());
        assert!(app_dir.join(MERGE_LOG_FILE).is_file());
        assert_eq!(
            fs::read_to_string(app_dir.join(MEMBERS_FILE)).unwrap(),
            ""
        );
        assert_eq!(
            fs::read_to_string(app_dir.join(MERGE_LOG_FILE)).unwrap(),
            ""
        );

        let listed = store.list(false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, app.id);
        assert_eq!(listed[0].title, "Plan the trip");

        let fetched = store.get(&app.id).unwrap().unwrap();
        assert_eq!(fetched, app);

        let _ = fs::remove_dir_all(&dir);
    }

    /// The acceptance criterion's create→list→get round-trip: it must run through
    /// a temp `RUBBERDUX_HOME`, exercising `FilesystemAppStore::new()`'s home
    /// resolution and rooting at `$RUBBERDUX_HOME/apps`. Serialized behind
    /// `ENV_LOCK` because it mutates the process-global env var.
    #[test]
    fn create_list_get_roundtrip_through_rubberdux_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let home = temp_apps_dir();
        let prev = std::env::var("RUBBERDUX_HOME").ok();
        // Edition 2024 marks env mutation `unsafe`; the established codebase idiom
        // (see `crate::session` tests) wraps the set/restore in `unsafe` while
        // `ENV_LOCK` keeps the global mutation serialized against other tests.
        unsafe {
            std::env::set_var("RUBBERDUX_HOME", &home);
        }

        let store = FilesystemAppStore::new();
        let app = sample_app("2026-06-10-00-00-07-UTC");
        store.create(&app).unwrap();

        // The store roots at `$RUBBERDUX_HOME/apps`.
        let app_dir = home.join("apps").join(app.id.as_str());
        assert!(app_dir.join(METADATA_FILE).is_file());
        assert!(app_dir.join(MEMBERS_FILE).is_file());
        assert!(app_dir.join(MERGE_LOG_FILE).is_file());

        let listed = store.list(false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, app.id);

        let fetched = store.get(&app.id).unwrap().unwrap();
        assert_eq!(fetched, app);

        // Restore the env var so other tests see the prior value.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("RUBBERDUX_HOME", v),
                None => std::env::remove_var("RUBBERDUX_HOME"),
            }
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn create_rejects_duplicate() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());
        let app = sample_app("2026-06-10-00-00-01-UTC");

        store.create(&app).unwrap();
        assert!(matches!(store.create(&app), Err(Error::App(_))));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_removes_from_default_list_but_stays_addressable() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());
        let app = sample_app("2026-06-10-00-00-02-UTC");
        store.create(&app).unwrap();

        store.archive(&app.id).unwrap();

        // Gone from the default listing.
        assert!(store.list(false).unwrap().is_empty());
        // Present when archives are included.
        let with_archived = store.list(true).unwrap();
        assert_eq!(with_archived.len(), 1);
        assert_eq!(with_archived[0].id, app.id);
        // Still addressable by id.
        assert_eq!(store.get(&app.id).unwrap().unwrap().id, app.id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_load_is_all_tombstoned() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());

        // Persist an App that claims to be Active on disk.
        let mut app = sample_app("2026-06-10-00-00-03-UTC");
        app.status = AppStatus::Active;
        let app_dir = dir.join(app.id.as_str());
        fs::create_dir_all(&app_dir).unwrap();
        fs::write(
            app_dir.join(METADATA_FILE),
            serde_json::to_string(&app).unwrap(),
        )
        .unwrap();

        // Startup load forces Tombstoned.
        let listed = store.list(false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, AppStatus::Tombstoned);
        assert_eq!(store.get(&app.id).unwrap().unwrap().status, AppStatus::Tombstoned);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_manifest_is_skipped_not_panicked() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());

        let good = sample_app("2026-06-10-00-00-04-UTC");
        store.create(&good).unwrap();

        // A directory with a broken manifest must be skipped, not crash.
        let bad_dir = dir.join("2026-06-10-00-00-05-UTC");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join(METADATA_FILE), b"{ not valid json").unwrap();

        let listed = store.list(false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, good.id);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn member_and_merge_logs_append() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());
        let app = sample_app("2026-06-10-00-00-06-UTC");
        store.create(&app).unwrap();

        store
            .record_member(
                &app.id,
                &MemberRecord {
                    session_id: "s1".into(),
                    recorded_at: "2026-06-10T00:00:00Z".into(),
                    joined: true,
                },
            )
            .unwrap();
        store
            .record_merge(
                &app.id,
                &MergeRecord {
                    source_app_id: "other".into(),
                    recorded_at: "2026-06-10T00:00:01Z".into(),
                },
            )
            .unwrap();

        let members = fs::read_to_string(dir.join(app.id.as_str()).join(MEMBERS_FILE)).unwrap();
        assert_eq!(members.lines().count(), 1);
        assert!(members.contains("\"s1\""));

        let merges = fs::read_to_string(dir.join(app.id.as_str()).join(MERGE_LOG_FILE)).unwrap();
        assert_eq!(merges.lines().count(), 1);
        assert!(merges.contains("\"other\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_on_missing_dir_is_empty() {
        let dir = temp_apps_dir();
        let store = FilesystemAppStore::with_apps_dir(dir.clone());
        assert!(store.list(false).unwrap().is_empty());
        assert!(store.list(true).unwrap().is_empty());
    }
}
