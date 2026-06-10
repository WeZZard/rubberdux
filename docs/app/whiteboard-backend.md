# App Domain — Whiteboard Backend

This is the design record for the persistent `App` domain (`src/app/`) and its
filesystem registry (`src/app/registry/`). It specifies the cluster entity that
backs every icon on the whiteboard and how that entity survives host restarts.
It is the subsystem-level companion to the cross-platform whiteboard
specification in [`docs/apps/whiteboard.md`](../apps/whiteboard.md); read that
first for the conceptual model (whiteboard, app, agent worker, tombstoning).

## Context

The cross-platform design says an App *is an agent* that the user observes and
approves, durable and addressable, with its own identity (icon + title) and a
spatial home on the board. Today the backend has no first-class App: agent
entries live in memory for one session (`src/agent/`, `src/gateway/state.rs`).

The whiteboard refines "an App is a single agent" into "an App is a **cluster**
that aggregates member sessions." The board groups related conversations under
one icon; the App is the cluster, and the sessions are its members. This
document covers only the persistent cluster entity and its on-disk registry —
not the supervisor, workers, gateway routes, identity/title-icon generation,
auto-merge clustering, or peer messaging, which are designed elsewhere.

## Model

An `App` (`src/app/mod.rs`) carries:

- `id: AppId` — a stable, creation-time-sortable identifier formatted like a
  session id (`YYYY-MM-DD-hh-mm-ss-UTC`), so it doubles as the on-disk directory
  name.
- `title: String` — the short descriptive title derived from the originating
  task.
- `icon: IconSpec` — a system-native **symbol glyph name plus a generated
  color** (an `#RRGGBB` hex string). This realizes whiteboard decision D3: no
  AI-generated illustrated icons.
- `position: BoardPosition` — the App's board cell (`row`, `column`). Spatial
  position carries memory, so it is persisted identity.
- `status: AppStatus` — `Active` (a worker runs) or `Tombstoned` (worker
  persisted and not running). This realizes whiteboard decision D4.
- `member_session_ids: Vec<String>` — the sessions clustered into this App.
- `summary: String` — the App's topic, summarized from its members.
- `user_locked: bool` — when set, the user has pinned membership so
  auto-clustering must not move this App's sessions.
- `last_active: String` — an RFC 3339 timestamp the board orders by
  (most-recently-used first).

Each session metadata record (`src/session.rs`, `AgentMetadata`) gains an
optional `app_id` backref pointing at the owning cluster. It is optional and
omitted when absent so existing session metadata keeps deserializing.

## Contract

`AppStore` (`src/app/registry/store.rs`) is the persistence trait:

- `create(&App)` — write a new App directory and manifest; error if it exists.
- `list(include_archived)` — return Apps; omit archived ones unless requested.
  Every returned App loads `Tombstoned` regardless of what is on disk: on host
  startup no worker is running yet, so the in-memory truth is always
  `Tombstoned` until the supervisor restores it.
- `get(&AppId)` — fetch one App, searching the live directory then the archive,
  so an archived App stays addressable.
- `archive(&AppId)` — move the App's directory under the archives subdir,
  removing it from `list(include_archived: false)`.
- `record_member` / `record_merge` — append a line to the App's `members.jsonl`
  / `merge_log.jsonl` log.

`FilesystemAppStore` implements this under `~/.rubberdux/apps/`, resolving
`RUBBERDUX_HOME` (default `~/.rubberdux`) exactly as
`crate::session::SessionManager` does. Per-App layout:

```text
~/.rubberdux/apps/{id}/metadata.json     # the App manifest (last-writer-wins)
~/.rubberdux/apps/{id}/members.jsonl      # append-only membership log
~/.rubberdux/apps/{id}/merge_log.jsonl    # append-only cluster-merge log
~/.rubberdux/apps/archives/{id}/          # archived Apps, excluded from default list
```

The JSONL append/read idiom mirrors
`crate::agent::runtime::history_store::FilesystemStore`: one JSON object per
line, opened with create+append. A directory whose `metadata.json` is missing or
malformed is **skipped with a warning**, never panicked over, so one corrupt App
cannot take down the whole listing.

## Decisions

Recorded ADR-style ([arc42 §9](https://docs.arc42.org/section-9/)): context,
decision, the alternative that lost, consequence.

### D1 — The App is a cluster of member sessions, not a single session

**Context.** The whiteboard groups related conversations under one icon, while
the existing backend has a session as its durable unit. **Decision.** The App is
a distinct cluster entity holding `member_session_ids`; sessions carry an
`app_id` backref. **Rejected.** Making the App an alias for one session — it
cannot represent a cluster, and auto-merge would have nowhere to attach. Also
rejected: a separate join table/index file — premature for a filesystem store
where the directory name already keys the App. **Consequence.** Clustering and
auto-merge have a first-class home; a session can name its owning App cheaply.

### D2 — A directory-per-App filesystem registry under `~/.rubberdux/apps/`

**Context.** Apps must persist across restarts; the project already stores
session data on the filesystem as JSONL and JSON manifests. **Decision.** One
directory per App keyed by `AppId`, with a `metadata.json` manifest plus
append-only `members.jsonl` and `merge_log.jsonl`, reusing the `session.rs`
home-resolution and `history_store.rs` JSONL idioms. **Rejected.** An embedded
database (SQLite) — heavier than needed and against the project's "no heavy
orchestration" rule; a single aggregate JSON file — rewriting it on every change
loses the append-only audit trail and risks whole-file corruption.
**Consequence.** Apps are inspectable on disk and consistent with the existing
session storage; membership and merges keep an append-only history.

### D3 — Manifest is the current snapshot; logs are the history

**Context.** Membership and merges both need a current view and an audit trail.
**Decision.** `metadata.json` is last-writer-wins for the current snapshot
(`member_session_ids`, `summary`, `position`, …); `members.jsonl` and
`merge_log.jsonl` are append-only records of how it got there. **Rejected.**
Reconstructing the snapshot by replaying the logs on every read — slower and
forces log readers to understand the full event model. **Consequence.** Reads
are a single manifest parse; the logs remain for provenance and future
reconciliation.

### D4 — Apps always load `Tombstoned`; archiving moves, it does not delete

**Context.** On startup no worker is running, and users need to retire Apps
without losing them. **Decision.** `list`/`get` force `status = Tombstoned` on
load regardless of the persisted value; `archive` renames the directory under
`apps/archives/{id}/` instead of deleting it. **Rejected.** Trusting the
persisted `status` on load — it would falsely show Apps as `Active` before any
worker exists; hard-deleting on archive — it would discard a durable unit the
user may want back. **Consequence.** In-memory status always reflects reality at
startup; archived Apps disappear from the default board yet stay addressable via
`get` and `list(include_archived: true)`.

## Out of Scope

Supervisor and worker lifecycle, gateway REST/WebSocket routes, title/icon and
summary generation, auto-clustering and the merge mechanism, peer messaging, and
all macOS/client code. This domain provides only the persistent `App` types, the
registry persistence, and the session backref that those subsystems build on.
