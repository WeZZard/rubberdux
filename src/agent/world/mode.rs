//! mode — see docs/agent/world/ecs-runtime.md

use serde::{Deserialize, Serialize};

use super::inputs::{Event, Origin};
use super::world::EdgeId;

// ---------------------------------------------------------------------------
// Mode — the three fluid UI interaction modes
// ---------------------------------------------------------------------------

/// The three fluid UI interaction modes. `Mode` is NEVER a stored field —
/// it is a PURE PROJECTION computed on demand by folding the `origin` of
/// recent `Event`s on a given edge (Invariant 19). Storing it would create
/// a second source of truth that can diverge from the log. The fold function
/// `mode` in this module is the sole entry point for that projection.
/// See docs/agent/world/ecs-runtime.md (Mode / mode-as-projection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Only human-origin inputs appear in the edge's recent event window.
    /// The agent is standing back; the human is in direct control.
    Operating,
    /// Human and agent-origin inputs are interleaved in the edge's recent
    /// event window. The agent is co-piloting alongside the human.
    Assisted,
    /// Agent-origin (or peer-origin) inputs dominate the edge's recent event
    /// window. The agent is steering; the human's presence is receding.
    Driven,
}

// ---------------------------------------------------------------------------
// mode — pure projection fold (Invariant 19)
// ---------------------------------------------------------------------------

/// Classify the interaction mode on `edge` by folding the `origin` of the
/// last `window` `Event`s whose `edge` field matches. The fold is a PURE
/// function: no I/O, no `now()`, no stored state — `&[Event]` in, `Mode` out.
///
/// Classification rules (see docs/agent/world/ecs-runtime.md, Mode-as-projection):
///
/// - `System`-origin events are **mode-neutral** and are never counted.
/// - Among the counted (actor) events in the window:
///   - No actor events at all → `Operating` (the explicit empty-window result).
///   - Only `Human` actors → `Operating`.
///   - Both `Human` and (`Agent` or `Peer`) present → `Assisted`.
///   - `Agent` or `Peer` present with no `Human` → `Driven`.
///     (`Driven` NEVER arises from mere Human-absence over neutral events.)
///
/// The fold is scoped to one edge; call it once per edge to obtain per-edge
/// modes that can differ simultaneously across edges (Invariant 19).
pub fn mode(events: &[Event], edge: EdgeId, window: usize) -> Mode {
    // Collect the last `window` events on this edge, then check actor origins.
    // System-origin events are silently skipped by not matching them below.
    let tail = events
        .iter()
        .filter(|e| e.edge == edge)
        .rev()
        .take(window);

    let mut has_human = false;
    let mut has_agent_or_peer = false;

    for e in tail {
        match e.origin {
            Origin::Human => has_human = true,
            Origin::Agent | Origin::Peer => has_agent_or_peer = true,
            Origin::System => {}
        }
    }

    match (has_human, has_agent_or_peer) {
        // Empty actor window or Human-only → Operating (Theme 3a).
        (false, false) | (true, false) => Mode::Operating,
        // Both Human and Agent/Peer present → Assisted (Theme 3b).
        (true, true) => Mode::Assisted,
        // Agent/Peer without Human → Driven; requires an actual actor sample
        // (cannot arise from neutral-only events since System never sets
        // has_agent_or_peer).
        (false, true) => Mode::Driven,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::world::inputs::{LogicalInput, Origin};

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    fn round_trip(mode: Mode) {
        let json = serde_json::to_string(&mode).expect("serialise mode");
        let back: Mode = serde_json::from_str(&json).expect("deserialise mode");
        assert_eq!(mode, back);
    }

    /// Build a minimal Event with the given origin and edge.
    fn evt(origin: Origin, edge: EdgeId) -> Event {
        Event {
            origin,
            edge,
            at: 0,
            wall: None,
            input: LogicalInput::Resume,
        }
    }

    // ---------------------------------------------------------------------------
    // Serde round-trip tests (pre-existing)
    // ---------------------------------------------------------------------------

    #[test]
    fn mode_operating_round_trips() {
        round_trip(Mode::Operating);
        let json = serde_json::to_string(&Mode::Operating).expect("serialise");
        assert_eq!(json, "\"operating\"");
    }

    #[test]
    fn mode_assisted_round_trips() {
        round_trip(Mode::Assisted);
        let json = serde_json::to_string(&Mode::Assisted).expect("serialise");
        assert_eq!(json, "\"assisted\"");
    }

    #[test]
    fn mode_driven_round_trips() {
        round_trip(Mode::Driven);
        let json = serde_json::to_string(&Mode::Driven).expect("serialise");
        assert_eq!(json, "\"driven\"");
    }

    #[test]
    fn mode_is_copy_and_eq() {
        let a = Mode::Operating;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(Mode::Operating, Mode::Driven);
    }

    // ---------------------------------------------------------------------------
    // mode() fold tests (Invariant 19 / VC-U.1)
    // ---------------------------------------------------------------------------

    /// Empty event list → Operating (the explicit empty-window result, Theme 3a).
    #[test]
    fn empty_window_yields_operating() {
        assert_eq!(mode(&[], 0, 10), Mode::Operating);
    }

    /// A window of only Human-origin events → Operating.
    #[test]
    fn only_human_window_yields_operating() {
        let events = [
            evt(Origin::Human, 0),
            evt(Origin::Human, 0),
            evt(Origin::Human, 0),
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Operating);
    }

    /// Human interleaved with Agent → Assisted (both actors present, Theme 3b).
    #[test]
    fn interleaved_human_and_agent_yields_assisted() {
        let events = [
            evt(Origin::Human, 0),
            evt(Origin::Agent, 0),
            evt(Origin::Human, 0),
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Assisted);
    }

    /// Agent-only window → Driven (Agent actor sample present, no Human).
    #[test]
    fn agent_only_window_yields_driven() {
        let events = [
            evt(Origin::Agent, 0),
            evt(Origin::Agent, 0),
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Driven);
    }

    /// Peer-only window → Driven (Peer actor sample present, no Human).
    #[test]
    fn peer_only_window_yields_driven() {
        let events = [
            evt(Origin::Peer, 0),
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Driven);
    }

    /// A window containing ONLY System/neutral events → Operating, NOT Driven.
    /// Proves Driven requires an actual Agent/Peer actor sample and NEVER arises
    /// from mere Human-absence over neutral events (Theme 3b, Invariant 19).
    #[test]
    fn neutral_only_window_yields_operating_not_driven() {
        let events = [
            evt(Origin::System, 0),
            evt(Origin::System, 0),
            evt(Origin::System, 0),
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Operating);
    }

    /// Two different edges in one event list fold to different modes simultaneously
    /// (concurrent per-edge modes, Invariant 19 / Theme 3c).
    #[test]
    fn concurrent_per_edge_modes() {
        let events = [
            evt(Origin::Human, 0),  // edge 0: human
            evt(Origin::Agent, 0),  // edge 0: agent → Assisted
            evt(Origin::Agent, 1),  // edge 1: agent only → Driven
            evt(Origin::Agent, 1),
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Assisted);
        assert_eq!(mode(&events, 1, 10), Mode::Driven);
    }

    /// The `window` parameter limits how far back the fold scans on the edge.
    /// If a Human event falls outside the window, only Agent events are visible
    /// and the result is Driven.
    #[test]
    fn window_size_limits_look_back() {
        // Three events on edge 0: old Human, then two Agents.
        let events = [
            evt(Origin::Human, 0), // tick 0 — oldest; outside a window of 2
            evt(Origin::Agent, 0), // tick 1
            evt(Origin::Agent, 0), // tick 2
        ];
        // Window of 2 → only the two Agent events are considered → Driven.
        assert_eq!(mode(&events, 0, 2), Mode::Driven);
        // Window of 3 → Human is included → Assisted.
        assert_eq!(mode(&events, 0, 3), Mode::Assisted);
    }

    /// Events on other edges do not pollute the fold for the queried edge.
    #[test]
    fn other_edge_events_are_ignored() {
        let events = [
            evt(Origin::Agent, 1), // edge 1, not edge 0
            evt(Origin::Human, 0), // edge 0 only
        ];
        assert_eq!(mode(&events, 0, 10), Mode::Operating);
    }
}
