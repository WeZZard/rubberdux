//! Filesystem persistence for the [`crate::app::App`] domain.
//!
//! The registry is where Apps live across host restarts: each App is a directory
//! under `~/.rubberdux/apps/{id}/` holding a `metadata.json` manifest plus the
//! append-only `members.jsonl` and `merge_log.jsonl` logs. The trait and its
//! filesystem implementation are in `store`; the design rationale is in
//! `docs/app/whiteboard-backend.md`.

pub mod store;

pub use store::{AppStore, FilesystemAppStore, MemberRecord, MergeRecord};
