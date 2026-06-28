//! edge — deterministic counterpart→edge binding (Theme 4a).
//! See docs/agent/world/ecs-runtime.md (Edge / Counterpart; Receiver edge
//! binding; `edge_for`; `EdgeBound`).
//!
//! Binding a [`Counterpart`] to a local [`EdgeId`] is a LOGGED fact, never an
//! out-of-band choice: an edge is keyed by the durable counterpart itself so a
//! receiver routes an inbound peer append to a stable, replayable edge in the LOG
//! rather than ad hoc. This module is the two halves of that rule, kept apart:
//!
//!  - the PURE LOOKUP CORE [`edge_for`] — a side-effect-free resolution over
//!    `Resources.edges`, and
//!  - the thin MINT-AND-LOG SEAM [`bind`] — which, on the FIRST use of a new
//!    counterpart, mints a fresh `EdgeId` and emits an [`EdgeBound`] Input the
//!    shell logs so a replay reproduces the identical binding (Inv 6).
//!
//! The Human and App edges are WELL-KNOWN conventional edges (`HUMAN_EDGE` /
//! `APP_EDGE`): they are pre-bound by convention and mint NOTHING and log NOTHING,
//! so refactoring the driver's previously-hardcoded edge routing through this
//! module changes no recorded byte — every existing recorded fixture and the
//! surface-mode replay byte-identity sinks stay byte-identical. ONLY a brand-new
//! `Peer` mints and logs an `EdgeBound`.
//!
//! [`EdgeBound`]: super::inputs::LogicalInput::EdgeBound

use super::inputs::LogicalInput;
use super::world::{Counterpart, Edge, EdgeId, Resources};

/// The human conversation edge — a WELL-KNOWN conventional edge, pre-bound by
/// convention to id 0. Host `UserMessage`s and CONVERSATION results (the model
/// call) route onto it. Because it is conventional it is NEVER recorded as an
/// `EdgeBound`: `edge_for(Counterpart::Human)` resolves it directly, so every
/// recorded session stays byte-identical to one written before edge binding
/// existed. See docs/agent/world/ecs-runtime.md (Mode-as-projection; `edge_for`).
pub const HUMAN_EDGE: EdgeId = 0;

/// The DISTINCT app/surface edge — a WELL-KNOWN conventional edge, pre-bound by
/// convention to id 1, kept separate from `HUMAN_EDGE` so the per-edge mode
/// projection (Inv 19) folds the agent's surface drive as `Driven` (agent-only on
/// this edge) while the human conversation edge stays `Assisted`. Conventional, so
/// likewise never recorded as an `EdgeBound`. See docs/agent/world/ecs-runtime.md
/// (Mode-as-projection; `edge_for`).
pub const APP_EDGE: EdgeId = 1;

/// The count of WELL-KNOWN conventional edge ids reserved up front (`HUMAN_EDGE`,
/// `APP_EDGE`). A minted peer edge starts AT or above this floor so it can never
/// collide with a conventional id even though the conventional edges are resolved
/// by convention and NOT stored in `Resources.edges` — mirroring
/// `IdAlloc::mint_entity` reserving entity 0 for the root agent.
const RESERVED_EDGES: EdgeId = 2;

/// PURE LOOKUP CORE (Theme 4a). Resolve a [`Counterpart`] to its bound [`EdgeId`]
/// over `Resources.edges` WITHOUT minting or logging — the side-effect-free half
/// of edge binding, kept separate from the mint-and-log seam [`bind`].
///
/// - `Human`/`App` are WELL-KNOWN conventional edges, always resolved to their
///   constants (`HUMAN_EDGE`/`APP_EDGE`) with no recorded binding, so routing the
///   driver's edges through this lookup changes no recorded byte.
/// - `Peer(_)` resolves to the edge previously bound for that EXACT counterpart in
///   `Resources.edges`, or `None` when it has never been bound — the signal the
///   caller uses to mint+log a fresh binding via [`bind`].
///
/// See docs/agent/world/ecs-runtime.md (`edge_for`; Theme 4a).
pub fn edge_for(resources: &Resources, counterpart: &Counterpart) -> Option<EdgeId> {
    match counterpart {
        Counterpart::Human => Some(HUMAN_EDGE),
        Counterpart::App => Some(APP_EDGE),
        // A peer edge is keyed by the durable counterpart itself (never by arrival
        // order), so the lookup scans for the edge whose record carries this exact
        // counterpart. `Resources.edges` is a `BTreeMap` (Inv 8), so the first
        // match is a deterministic function of World state.
        Counterpart::Peer(_) => resources
            .edges
            .iter()
            .find(|(_, edge)| &edge.counterpart == counterpart)
            .map(|(id, _)| *id),
    }
}

/// THE MINT-AND-LOG SEAM (Theme 4a) — the thin, shell-invoked half of edge
/// binding. Resolve a counterpart to its `EdgeId`, returning alongside it an
/// [`EdgeBound`] Input to LOG when (and only when) the binding is genuinely NEW so
/// a replay reproduces the identical `EdgeId` (Inv 6).
///
/// The second tuple element is `None` whenever nothing new must be logged:
/// - the WELL-KNOWN conventional edges (`Human`/`App`) are pre-bound by
///   convention — they mint NOTHING and log NOTHING, the property that keeps every
///   existing recorded fixture byte-identical, and
/// - an already-bound `Peer` reuses its recorded `EdgeId`.
///
/// It is `Some(EdgeBound { edge, counterpart })` ONLY for the FIRST use of a new
/// `Peer`: the seam MINTS a fresh `EdgeId` (reserved above the conventional ids)
/// as a pure function of `Resources` so the mint is itself replayable. The caller
/// logs the returned Input and folds it through [`fold_edge_bound`] (which records
/// the binding into `Resources.edges` and advances the allocator); thereafter
/// [`edge_for`] returns the bound edge. Calling `bind` again for the same unbound
/// peer before folding yields the SAME minted id (a pure function of `Resources`).
///
/// See docs/agent/world/ecs-runtime.md (`edge_for`; `EdgeBound`; Theme 4a).
///
/// [`EdgeBound`]: super::inputs::LogicalInput::EdgeBound
pub fn bind(resources: &Resources, counterpart: &Counterpart) -> (EdgeId, Option<LogicalInput>) {
    if let Some(edge) = edge_for(resources, counterpart) {
        // Already bound (well-known or a recorded peer): nothing new to log.
        return (edge, None);
    }
    let edge = mint_peer_edge(resources);
    (
        edge,
        Some(LogicalInput::EdgeBound {
            edge,
            counterpart: counterpart.clone(),
        }),
    )
}

/// Fold an `EdgeBound` into `Resources` — the deterministic APPLICATION of a
/// binding that makes it survive replay. It records `Edge { id, counterpart }`
/// into `Resources.edges` and advances the id allocator past the bound edge so a
/// later peer mints a fresh id. Idempotent on the `EdgeId` (re-applying the same
/// binding is a no-op), so a redelivered or duplicated `EdgeBound` never diverges
/// the World. Pure: `Resources` in, `Resources` out. See
/// docs/agent/world/ecs-runtime.md (`EdgeBound`; Theme 4a).
//
// The production caller is the pure `tick` reducer (`systems::tick`): it folds a
// recorded `EdgeBound` into `Resources` identically on the LIVE and replay paths —
// the inbound peer-edge wiring this milestone lands. The well-known human/app
// conventional edges mint and fold nothing, so this runs only for a fresh `Peer`.
pub fn fold_edge_bound(
    resources: &Resources,
    edge: EdgeId,
    counterpart: &Counterpart,
) -> Resources {
    let mut resources = resources.clone();
    resources.edges.entry(edge).or_insert_with(|| Edge {
        id: edge,
        counterpart: counterpart.clone(),
    });
    // Keep the allocator's next edge strictly past every bound id so a future
    // `bind` mints a fresh, collision-free `EdgeId` on both the live and replay
    // paths (the live run and a reconstruction advance identically).
    if resources.ids.next_edge <= edge {
        resources.ids.next_edge = edge + 1;
    }
    resources
}

/// Mint the next peer `EdgeId` as a pure function of `Resources`: the allocator's
/// next edge, floored at `RESERVED_EDGES` so it can never collide with a
/// conventional id (the conventional edges are resolved by convention, not stored
/// in `Resources.edges`). Mirrors `IdAlloc::mint_entity` reserving entity 0.
fn mint_peer_edge(resources: &Resources) -> EdgeId {
    resources.ids.next_edge.max(RESERVED_EDGES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::world::{Effort, ModelConfig, PeerId};

    fn resources() -> Resources {
        Resources::new(
            7,
            ModelConfig {
                model: "claude-x".into(),
                max_tokens: 1024,
                effort: Effort::Medium,
            },
        )
    }

    fn peer(node: &str) -> Counterpart {
        Counterpart::Peer(PeerId {
            app_id: "app".into(),
            node_id: node.into(),
        })
    }

    /// The well-known conventional edges resolve to their constants WITHOUT any
    /// recorded binding — the byte-identity guarantee for human/app.
    #[test]
    fn well_known_edges_resolve_without_logging() {
        let res = resources();

        assert_eq!(edge_for(&res, &Counterpart::Human), Some(HUMAN_EDGE));
        assert_eq!(edge_for(&res, &Counterpart::App), Some(APP_EDGE));

        // `bind` returns the conventional id and NEVER an `EdgeBound` for them.
        assert_eq!(bind(&res, &Counterpart::Human), (HUMAN_EDGE, None));
        assert_eq!(bind(&res, &Counterpart::App), (APP_EDGE, None));
    }

    /// The FIRST use of a new peer mints an `EdgeId` (reserved above the
    /// conventional ids) and emits exactly one `EdgeBound` carrying that id.
    #[test]
    fn first_use_of_a_peer_mints_and_logs_an_edge_bound() {
        let res = resources();
        let p = peer("n1");

        // Not yet bound — the lookup core has nothing to return.
        assert_eq!(edge_for(&res, &p), None);

        let (edge, logged) = bind(&res, &p);
        // Minted above the two conventional ids so it can never collide with 0/1.
        assert!(edge >= RESERVED_EDGES, "a peer edge is reserved above the conventional ids");
        assert_eq!(
            logged,
            Some(LogicalInput::EdgeBound {
                edge,
                counterpart: p.clone(),
            }),
            "the first use of a new peer emits an EdgeBound carrying the minted id"
        );
    }

    /// After the binding is folded, a re-resolution REUSES the same `EdgeId` and
    /// emits NO second `EdgeBound` — `edge_for` returns the bound edge from
    /// `Resources.edges` thereafter (Inv 6).
    #[test]
    fn rebinding_reuses_the_edge_id_without_a_second_log() {
        let res = resources();
        let p = peer("n1");

        let (edge, logged) = bind(&res, &p);
        let LogicalInput::EdgeBound { edge: bound, counterpart } =
            logged.expect("first use logs an EdgeBound")
        else {
            unreachable!("bind of a new peer yields an EdgeBound");
        };
        let res = fold_edge_bound(&res, bound, &counterpart);

        // The lookup core now resolves the same id with no logging.
        assert_eq!(edge_for(&res, &p), Some(edge));
        assert_eq!(
            bind(&res, &p),
            (edge, None),
            "a re-resolved peer reuses its EdgeId and logs no second EdgeBound"
        );
    }

    /// A second DISTINCT peer mints a fresh, distinct `EdgeId` after the first is
    /// folded — the allocator advances past every bound id.
    #[test]
    fn distinct_peers_get_distinct_edges() {
        let res = resources();
        let (e1, log1) = bind(&res, &peer("n1"));
        let res = match log1 {
            Some(LogicalInput::EdgeBound { edge, counterpart }) => {
                fold_edge_bound(&res, edge, &counterpart)
            }
            _ => unreachable!("first peer logs an EdgeBound"),
        };

        let (e2, log2) = bind(&res, &peer("n2"));
        assert_ne!(e1, e2, "a distinct peer binds to a distinct edge");
        assert!(
            matches!(log2, Some(LogicalInput::EdgeBound { edge, .. }) if edge == e2),
            "the second peer logs its own EdgeBound"
        );
    }

    /// Folding the SAME `EdgeBound` twice is a no-op (idempotent) — a redelivered
    /// binding never diverges the World, the property a faithful replay relies on.
    #[test]
    fn folding_the_same_binding_twice_is_idempotent() {
        let res = resources();
        let p = peer("n1");
        let (edge, _) = bind(&res, &p);

        let once = fold_edge_bound(&res, edge, &p);
        let twice = fold_edge_bound(&once, edge, &p);
        assert_eq!(once, twice, "re-applying a binding is a no-op");
    }
}
