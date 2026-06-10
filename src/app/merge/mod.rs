//! Auto-merge clustering — deciding whether a new conversation joins an
//! existing App or forms a new one. See `docs/app/merge/clustering.md`.
//!
//! The public surface is the [`Clusterer`] trait (the decision mechanism) and
//! the [`ClusterDecision`] it returns. The concrete LLM-backed realization with
//! a lexical Jaccard pre-filter and offline fallback lives in
//! [`clustering`](self::clustering). The gateway consults a `Clusterer` off the
//! `POST /apps` path on a background task so the request never blocks on the
//! decision.

pub mod clustering;

use serde::{Deserialize, Serialize};

/// A candidate App the clusterer may merge a new conversation into. Only the
/// fields the decision needs are carried, so the clusterer stays a pure function
/// over summaries and never reaches into the store or supervisor itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterCandidate {
    /// The candidate App's id, returned untouched in a [`ClusterDecision::Join`]
    /// so the caller knows which App to merge into.
    pub app_id: String,
    /// The candidate App's topic summary, compared against the new
    /// conversation's summary.
    pub summary: String,
}

/// The outcome of a clustering decision: either merge the new conversation into
/// an existing candidate App, or create a new App for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ClusterDecision {
    /// Merge into the candidate App with this id.
    Join { app_id: String },
    /// No suitable existing App — create a new one.
    New,
}

/// The clustering decision mechanism. Implementations decide whether a new (or
/// drifted) conversation summary belongs with an existing App. The trait is pure
/// with respect to side effects: it reads only the summaries it is handed and
/// returns a decision; the caller owns persistence, routing, and `user_locked`
/// filtering. See `docs/app/merge/clustering.md`.
pub trait Clusterer: Send + Sync {
    /// Decide whether a brand-new conversation joins one of `candidates` or
    /// forms a new App. `candidates` must already exclude `user_locked` Apps —
    /// the clusterer never overrides a user's pin.
    fn classify(
        &self,
        new_summary: &str,
        candidates: &[ClusterCandidate],
    ) -> impl std::future::Future<Output = ClusterDecision> + Send;

    /// Re-decide membership for a conversation already inside `current_app_id`
    /// after its topic has drifted (e.g. once the first agent turn refines the
    /// summary). Returns the same decision shape; the caller applies any move.
    /// `candidates` must already exclude `user_locked` Apps. Documented here so
    /// the active-management surface exists; scheduling when this runs is the
    /// host-wiring task's concern. See `docs/app/merge/clustering.md`.
    fn reevaluate(
        &self,
        member_summary: &str,
        current_app_id: &str,
        candidates: &[ClusterCandidate],
    ) -> impl std::future::Future<Output = ClusterDecision> + Send;
}
