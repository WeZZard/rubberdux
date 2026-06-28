//! Edge-binding integration tests — VC-2.1 (binding logged once, replay-
//! reproducible) and VC-2.2 (peer-origin input on the peer edge projects to
//! `Driven` mode).
//!
//! **VC-2.1** proves, at the `Resources`-level:
//!
//! - `bind(Peer)` mints an `EdgeId` reserved above the two well-known
//!   conventional ids and emits EXACTLY ONE `EdgeBound` on the first use of
//!   that counterpart.
//! - A re-resolution of the same peer reuses the SAME `EdgeId` and emits NO
//!   second `EdgeBound`.
//! - Two DISTINCT peers get distinct edges; the allocator advances past each
//!   folded binding.
//! - Folding the logged `EdgeBound` — after it survives a serde ROUND-TRIP
//!   across the log boundary — reconstructs the binding that the LIVE `bind`
//!   minted (Inv 6): the reconstructed `Resources.edges` and allocator match an
//!   INDEPENDENT hand-built reference, and `edge_for` resolves the peer back to
//!   the id `bind` minted on the live side (not merely to itself).
//!
//! **VC-2.2** proves, at the `mode`-projection level:
//!
//! - An `Origin::Peer` input on the edge bound to `Peer(from)` projects that
//!   edge to `Driven` mode — mirroring the App edge's Agent-driven `Driven`
//!   result.  The `mode()` function classifies `Origin::Peer` as
//!   `has_agent_or_peer`, so the Driven projection is available immediately,
//!   without waiting for PeerDriveSystem (PA-system).
//!
//! No live model call — all assertions are pure folds over synthetic data.
//! See docs/agent/world/ecs-runtime.md — Edge binding (Theme 4a), Inv 6,
//! Inv 19.

use std::collections::BTreeMap;

use rubberdux::agent::world::edge::{APP_EDGE, HUMAN_EDGE, bind, edge_for, fold_edge_bound};
use rubberdux::agent::world::inputs::{Event, LogicalInput, Origin};
use rubberdux::agent::world::mode::{Mode, mode};
use rubberdux::agent::world::world::{Counterpart, Edge, EdgeId, Effort, ModelConfig, PeerId, Resources};

// The mode look-back window — larger than any event list used here so every
// authored event is in view (the window is not the subject under test; the
// origin fold is).
const WINDOW: usize = 16;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn model() -> ModelConfig {
    ModelConfig {
        model: "claude-edge-binding-test".into(),
        max_tokens: 1024,
        effort: Effort::Medium,
    }
}

/// Fresh `Resources` with no edges bound — the starting point for every test.
fn resources() -> Resources {
    Resources::new(7, model())
}

/// A `Counterpart::Peer` identified by `app_id` + `node_id`.
fn peer(app_id: &str, node_id: &str) -> Counterpart {
    Counterpart::Peer(PeerId {
        app_id: app_id.into(),
        node_id: node_id.into(),
    })
}

/// A minimal `Event` carrying `Origin::Peer` on `edge` at logical tick `at`.
/// The input payload is `Resume` — a valid exogenous input whose content is
/// mode-neutral (only the `origin` field matters for mode projection).
fn peer_event(edge: u32, at: u64) -> Event {
    Event {
        origin: Origin::Peer,
        edge,
        at,
        wall: None,
        input: LogicalInput::Resume,
    }
}

// ---------------------------------------------------------------------------
// VC-2.1 — edge binding is logged once and replay-reproducible
// ---------------------------------------------------------------------------

/// [VC-2.1] The FIRST use of a new peer mints an `EdgeId` strictly above the
/// two conventional well-known ids (`HUMAN_EDGE=0`, `APP_EDGE=1`) and emits
/// EXACTLY ONE `EdgeBound` carrying that id and the counterpart.
#[test]
fn vc_2_1_first_peer_use_mints_exactly_one_edge_bound() {
    let res = resources();
    let p = peer("app-a", "node-1");

    // Precondition: not yet bound.
    assert_eq!(edge_for(&res, &p), None, "a fresh peer has no bound edge");

    let (edge_id, logged) = bind(&res, &p);

    // The minted id must be reserved strictly above both well-known ids so it
    // can never collide with the conventional human/app edges.
    assert!(
        edge_id > APP_EDGE,
        "peer edge (id={edge_id}) must be > APP_EDGE={APP_EDGE} > HUMAN_EDGE={HUMAN_EDGE}"
    );

    // Exactly ONE `EdgeBound` is emitted, carrying the minted edge and the
    // exact counterpart.
    match logged {
        Some(LogicalInput::EdgeBound { edge, counterpart }) => {
            assert_eq!(edge, edge_id, "EdgeBound carries the minted EdgeId");
            assert_eq!(counterpart, p, "EdgeBound carries the exact Counterpart");
        }
        other => panic!("expected Some(EdgeBound), got {other:?}"),
    }
}

/// [VC-2.1] After the binding is folded into `Resources`, a re-resolution of
/// the SAME peer reuses the SAME `EdgeId` and emits NO second `EdgeBound`.
#[test]
fn vc_2_1_re_resolution_reuses_edge_id_no_second_edge_bound() {
    let res = resources();
    let p = peer("app-a", "node-1");

    // First use: mint and fold.
    let (edge_id, logged) = bind(&res, &p);
    let LogicalInput::EdgeBound { edge, counterpart } =
        logged.expect("first use logs an EdgeBound")
    else {
        panic!("expected EdgeBound variant");
    };
    let res_bound = fold_edge_bound(&res, edge, &counterpart);

    // `edge_for` resolves the bound edge.
    assert_eq!(
        edge_for(&res_bound, &p),
        Some(edge_id),
        "edge_for resolves the peer to its EdgeId after fold"
    );

    // Second `bind` call — must reuse the id and NOT emit another EdgeBound.
    let (second_edge, second_logged) = bind(&res_bound, &p);
    assert_eq!(
        second_edge, edge_id,
        "re-resolution returns the SAME EdgeId (no re-mint)"
    );
    assert_eq!(
        second_logged, None,
        "a re-resolved peer emits NO second EdgeBound"
    );
}

/// [VC-2.1] Two DISTINCT peers each receive their own distinct `EdgeId`; the
/// allocator advances past the first binding before minting the second.
#[test]
fn vc_2_1_distinct_peers_get_distinct_edge_ids() {
    let res = resources();
    let p1 = peer("app-a", "node-1");
    let p2 = peer("app-a", "node-2");

    // Mint and fold the first peer.
    let (e1, log1) = bind(&res, &p1);
    let LogicalInput::EdgeBound { edge: edge1, counterpart: cp1 } =
        log1.expect("first peer logs an EdgeBound")
    else {
        panic!("expected EdgeBound");
    };
    let res1 = fold_edge_bound(&res, edge1, &cp1);

    // Mint the second peer from the updated Resources.
    let (e2, log2) = bind(&res1, &p2);
    assert_ne!(e1, e2, "distinct peers get distinct EdgeIds");
    assert!(
        matches!(&log2, Some(LogicalInput::EdgeBound { edge, .. }) if *edge == e2),
        "second peer logs its own EdgeBound carrying the new EdgeId"
    );

    // Fold the second binding and verify both resolve correctly.
    let res2 = match log2 {
        Some(LogicalInput::EdgeBound { edge, counterpart }) => {
            fold_edge_bound(&res1, edge, &counterpart)
        }
        _ => unreachable!("second peer logs EdgeBound"),
    };
    assert_eq!(
        edge_for(&res2, &p1),
        Some(e1),
        "peer-1 still resolves to its original EdgeId after folding peer-2"
    );
    assert_eq!(
        edge_for(&res2, &p2),
        Some(e2),
        "peer-2 resolves to its own distinct EdgeId"
    );
}

/// [VC-2.1] The binding reconstructed from the logged `EdgeBound` reproduces the
/// LIVE binding `bind` minted (Inv 6 — deterministic replay), checked against an
/// INDEPENDENT reference rather than against another fold of the same record.
///
/// "Replay" here crosses the actual log boundary and never folds the record
/// against itself:
///
/// 1. LIVE: `bind` mints the edge and emits the `EdgeBound` log record. The
///    minted `EdgeId` is captured (NOT discarded) as the independent witness of
///    what the live side produced.
/// 2. LOG BOUNDARY: the `EdgeBound`, wrapped in its `Event` envelope, is
///    serialised to JSON and parsed back — proving the record survives the exact
///    serde round-trip the JSONL log uses.
/// 3. REPLAY: `fold_edge_bound` applies the ROUND-TRIPPED record from a fresh
///    `Resources`, modelling a receiver reconstructing the log.
/// 4. INDEPENDENT CHECK: the reconstructed `Resources.edges` and `ids.next_edge`
///    are asserted against a HAND-BUILT reference (`{minted -> Edge{minted,
///    Peer}}`, `next_edge == minted + 1`), and `edge_for` must resolve the peer
///    back to the LIVE-minted id. A fold that stored a wrong id/counterpart or
///    mis-advanced the allocator diverges from the hand-built reference (step 4)
///    or the minted id, so the test FAILS — it is not `f(x) == f(x)`.
#[test]
fn vc_2_1_logged_edge_bound_replays_to_identical_binding_byte_for_byte() {
    let res0 = resources();
    let p = peer("app-replay", "node-r");

    // (1) LIVE: bind mints the edge and emits the EdgeBound to log. Capture the
    //     minted id as the INDEPENDENT witness — never discard it.
    let (minted_edge, logged) = bind(&res0, &p);
    let logged_input = logged.expect("first use logs an EdgeBound — this IS the log record");

    // (2) LOG BOUNDARY: wrap the record in its Event envelope (origin Peer, on
    //     the minted edge) and round-trip it through the SAME JSON serialisation
    //     the JSONL log uses, then extract the round-tripped edge/counterpart.
    let log_line = Event {
        origin: Origin::Peer,
        edge: minted_edge,
        at: 0,
        wall: None,
        input: logged_input,
    };
    let bytes = serde_json::to_vec(&log_line).expect("serialise EdgeBound log line");
    let decoded: Event =
        serde_json::from_slice(&bytes).expect("deserialise EdgeBound log line");
    let LogicalInput::EdgeBound { edge: rt_edge, counterpart: rt_counterpart } = decoded.input
    else {
        panic!("expected EdgeBound after the log round-trip");
    };
    // The log boundary preserved the record verbatim.
    assert_eq!(rt_edge, minted_edge, "the log round-trip preserves the minted EdgeId");
    assert_eq!(rt_counterpart, p, "the log round-trip preserves the exact Counterpart");

    // (3) REPLAY: fold the ROUND-TRIPPED record from a fresh Resources.
    let res_replay = fold_edge_bound(&res0, rt_edge, &rt_counterpart);

    // (4) INDEPENDENT CHECK: the reconstructed edge table and allocator match a
    //     HAND-BUILT reference — not another fold of the same record. A wrong
    //     stored id/counterpart fails the edges comparison; a mis-advanced
    //     allocator fails the next_edge comparison.
    let mut expected_edges: BTreeMap<EdgeId, Edge> = BTreeMap::new();
    expected_edges.insert(
        minted_edge,
        Edge {
            id: minted_edge,
            counterpart: p.clone(),
        },
    );
    assert_eq!(
        res_replay.edges, expected_edges,
        "the replayed edge table matches the hand-built reference \
         {{minted -> Edge{{minted, Peer}}}} (catches a wrong stored id/counterpart)"
    );
    assert_eq!(
        res_replay.ids.next_edge,
        minted_edge + 1,
        "the allocator advanced exactly past the minted edge (catches a mis-advanced allocator)"
    );

    // bind -> fold -> edge_for closes the loop: the replay resolves the peer back
    // to the id the LIVE bind minted (the value the original code discarded).
    assert_eq!(
        edge_for(&res_replay, &p),
        Some(minted_edge),
        "edge_for on the replay resolves the peer to the LIVE-minted EdgeId"
    );
}

/// [VC-2.1] `fold_edge_bound` is IDEMPOTENT: re-applying the same `EdgeBound`
/// twice does not diverge the binding (a redelivered log record is safe and
/// the World never diverges from a faithful replay, Inv 6).
#[test]
fn vc_2_1_fold_edge_bound_is_idempotent() {
    let res = resources();
    let p = peer("app-idem", "node-i");

    let (edge_id, _) = bind(&res, &p);
    let once = fold_edge_bound(&res, edge_id, &p);
    let twice = fold_edge_bound(&once, edge_id, &p);

    assert_eq!(
        once, twice,
        "re-applying the same EdgeBound is a no-op (idempotent, Inv 6)"
    );
    assert_eq!(
        edge_for(&twice, &p),
        Some(edge_id),
        "edge_for still resolves after a duplicate fold"
    );

    // Byte-identical serialisation confirms no hidden state drift.
    assert_eq!(
        serde_json::to_vec(&once).expect("serialise once"),
        serde_json::to_vec(&twice).expect("serialise twice"),
        "idempotent fold yields BYTE-IDENTICAL Resources"
    );
}

// ---------------------------------------------------------------------------
// VC-2.2 — a peer-origin input on the peer edge projects to Driven mode
// ---------------------------------------------------------------------------

/// [VC-2.2] An `Origin::Peer` input on the edge bound to `Peer(from)` projects
/// that edge to `Driven` mode — mirroring how the App edge projects to `Driven`
/// when only Agent-origin inputs appear on it (world_surface_mode_replay.rs,
/// tick 8, `EDGE_DRIVEN`).
///
/// This is testable NOW because `mode()` already classifies `Origin::Peer` as
/// `has_agent_or_peer` (mode.rs:67). No PeerDriveSystem (PA-system) is needed
/// for the mode-projection assertion: the projection is a pure fold over event
/// origins, independent of how the inbound peer input is processed in the World.
///
/// The peer edge is NOT a hardcoded literal: it is obtained the SAME way the
/// runtime resolves an inbound peer — `bind(Peer(from))` then `edge_for` — so the
/// events under test sit on the id the binding actually minted (VC-2.1's binding
/// feeds VC-2.2's projection, exactly as the criterion states). A binding bug that
/// placed the peer on a wrong or colliding edge id would move the assertion target
/// and surface here.
///
/// The test also guards the boundary conditions:
/// - a HUMAN-only edge stays `Operating`,
/// - the conventional AGENT/App edge stays `Driven` (the mirror),
/// - the peer edge WITH an interleaved Human event folds to `Assisted` (not
///   `Driven`), proving the result requires a real Peer actor sample and is
///   not a vacuous absence of Human events.
#[test]
fn vc_2_2_peer_origin_input_on_peer_edge_projects_to_driven_mode() {
    // Resolve the peer edge the runtime way: bind a real peer, fold its
    // EdgeBound, then read back the minted id via edge_for(Peer(from)). This ties
    // VC-2.1's binding to the projection below — the events sit on the BOUND id,
    // not an assumed literal.
    let res = resources();
    let from = peer("app-driver", "node-lead");
    let (bound_edge, _log) = bind(&res, &from);
    let res = fold_edge_bound(&res, bound_edge, &from);
    let peer_edge = edge_for(&res, &from).expect("the bound peer resolves to its edge");

    // The mirror is the conventional Agent-driven App edge; the human-only edge is
    // the conventional Human edge. Both must be DISTINCT from the minted peer edge
    // so the three per-edge modes are independent (Inv 19).
    assert_ne!(peer_edge, APP_EDGE, "the minted peer edge is distinct from the App edge");
    assert_ne!(peer_edge, HUMAN_EDGE, "the minted peer edge is distinct from the Human edge");

    let events: Vec<Event> = vec![
        // Human-only edge.
        Event {
            origin: Origin::Human,
            edge: HUMAN_EDGE,
            at: 0,
            wall: None,
            input: LogicalInput::Resume,
        },
        // Agent-only (App) edge — the mirror: Agent-driven coexists with Peer-driven.
        Event {
            origin: Origin::Agent,
            edge: APP_EDGE,
            at: 1,
            wall: None,
            input: LogicalInput::Resume,
        },
        // Peer-origin inputs on the RESOLVED peer edge — the assertion target.
        peer_event(peer_edge, 2),
        peer_event(peer_edge, 3),
    ];

    // (1) Human-only edge → Operating (same rule as EDGE_OPERATING in
    //     world_surface_mode_replay.rs).
    assert_eq!(
        mode(&events, HUMAN_EDGE, WINDOW),
        Mode::Operating,
        "a human-only edge folds to Operating"
    );

    // (2) Mirror: the Agent-driven App edge → Driven (same rule as EDGE_DRIVEN in
    //     world_surface_mode_replay.rs tick 8).
    assert_eq!(
        mode(&events, APP_EDGE, WINDOW),
        Mode::Driven,
        "the Agent-driven App edge folds to Driven (mirror)"
    );

    // (3) VC-2.2 ASSERTION: Origin::Peer inputs on edge_for(Peer(from)) → Driven.
    //     The edge is the one the binding minted (not a literal), so this is the
    //     criterion's `edge_for(Peer(from))` clause; Origin::Peer is mode-
    //     equivalent to Origin::Agent (mode.rs), so with no Human present the
    //     bound peer edge folds to Driven — mirroring the App edge.
    assert_eq!(
        mode(&events, peer_edge, WINDOW),
        Mode::Driven,
        "Origin::Peer inputs on the BOUND peer edge fold to Driven"
    );

    // (4) Boundary guard: the peer edge WITH a Human input → Assisted (Driven
    //     requires an actual Peer actor sample, not mere Human-absence).
    let mixed_events: Vec<Event> = events
        .iter()
        .cloned()
        .chain(std::iter::once(Event {
            origin: Origin::Human,
            edge: peer_edge,
            at: 4,
            wall: None,
            input: LogicalInput::Resume,
        }))
        .collect();
    assert_eq!(
        mode(&mixed_events, peer_edge, WINDOW),
        Mode::Assisted,
        "a peer edge with an interleaved Human input folds to Assisted, not Driven"
    );
}
