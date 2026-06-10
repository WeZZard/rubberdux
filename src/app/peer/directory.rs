//! The dynamic directory of Apps reachable on the peer network right now.
//!
//! The directory is the answer to "who can I talk to right now?". It is
//! *dynamic*: an App registers when its worker becomes active, refreshes its
//! position on every message it sends or receives (most-recently-used ordering),
//! and is removed when its worker stops. Listing returns the live peers in
//! most-recently-used order, so the freshest collaborators surface first.
//!
//! The directory carries no policy of its own beyond a single [`may_send`] gate
//! that keeps an App from addressing itself; the host consults it to resolve a
//! target and otherwise makes no routing decisions (it is a switch, not an
//! orchestrator — see `docs/app/peer/decentralized-messaging.md`).

use std::collections::HashMap;

use crate::app::peer::PeerId;

/// One live entry in the [`PeerDirectory`]: a reachable peer plus a monotonic
/// recency rank used to order the directory most-recently-used first. A larger
/// `rank` is more recent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PeerEntry {
    id: PeerId,
    rank: u64,
}

/// A dynamic registry of the peers reachable on the network at this moment.
///
/// Registration and refresh are driven by real activity: a worker that becomes
/// active registers; every message it sends or receives refreshes its recency.
/// A stopped worker is unregistered. The directory therefore always reflects the
/// *current* reachable set, never a stale snapshot.
///
/// Recency is tracked by a monotonically increasing counter rather than a
/// wall-clock timestamp so ordering is deterministic and independent of clock
/// resolution — two refreshes in the same instant still order correctly.
#[derive(Debug, Default)]
pub struct PeerDirectory {
    entries: HashMap<PeerId, PeerEntry>,
    /// Monotonic source of recency ranks. Incremented on every register/refresh
    /// so the most recent activity always has the highest rank.
    clock: u64,
}

impl PeerDirectory {
    /// An empty directory with no reachable peers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a peer as reachable, or refresh an already-registered peer's
    /// recency. Called when a worker becomes active and on every message so the
    /// directory is most-recently-used ordered. Idempotent: registering a peer
    /// that is already present only bumps its recency.
    pub fn register(&mut self, id: PeerId) {
        self.clock += 1;
        let rank = self.clock;
        self.entries
            .entry(id.clone())
            .and_modify(|entry| entry.rank = rank)
            .or_insert(PeerEntry { id, rank });
    }

    /// Refresh a peer's most-recently-used position. An alias of [`register`] for
    /// the call sites that semantically *touch* an existing peer (e.g. on each
    /// message) rather than admit a new one; both keep the directory ordered.
    pub fn touch(&mut self, id: &PeerId) {
        // Refreshing a peer that has since dropped re-admits it, which is the
        // intended most-recently-used behavior: activity makes a peer reachable.
        self.register(id.clone());
    }

    /// Remove a peer from the reachable set, called when its worker stops. A
    /// no-op for a peer that is not registered. Removal does not affect
    /// addressability — a message to a now-unregistered App is still queued to
    /// its inbox by the broker and woken on delivery.
    pub fn unregister(&mut self, id: &PeerId) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Whether a peer is currently registered as reachable.
    pub fn contains(&self, id: &PeerId) -> bool {
        self.entries.contains_key(id)
    }

    /// The number of reachable peers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The reachable peers in most-recently-used order (most recent first). This
    /// is the answer to "who can I talk to right now?".
    pub fn list(&self) -> Vec<PeerId> {
        let mut entries: Vec<&PeerEntry> = self.entries.values().collect();
        // Highest rank (most recent) first; the rank is unique per register/touch
        // so the order is total and deterministic. Sort by the negated key so the
        // most-recently-used peer leads.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.rank));
        entries.into_iter().map(|entry| entry.id.clone()).collect()
    }

    /// The peers `from` may address, in most-recently-used order: every reachable
    /// peer except `from` itself. The single directory-level policy; the broker
    /// applies no further gate (it is a switch, not an orchestrator).
    pub fn addressable_by(&self, from: &PeerId) -> Vec<PeerId> {
        self.list()
            .into_iter()
            .filter(|id| self.may_send(from, id))
            .collect()
    }

    /// Whether `from` may send to `to`. The only rule is that an App may not
    /// address itself; a peer that is offline is still a valid target (the broker
    /// queues to its inbox), so reachability is deliberately *not* part of this
    /// gate. Archiving and tombstoning are likewise orthogonal to addressability.
    pub fn may_send(&self, from: &PeerId, to: &PeerId) -> bool {
        from != to
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppId;
    use crate::app::peer::NodeId;

    fn peer(app: &str) -> PeerId {
        PeerId::local(AppId(app.into()))
    }

    #[test]
    fn register_admits_a_peer() {
        let mut dir = PeerDirectory::new();
        assert!(dir.is_empty());
        dir.register(peer("a"));
        assert_eq!(dir.len(), 1);
        assert!(dir.contains(&peer("a")));
    }

    #[test]
    fn register_is_idempotent_on_identity() {
        let mut dir = PeerDirectory::new();
        dir.register(peer("a"));
        dir.register(peer("a"));
        assert_eq!(dir.len(), 1, "re-registering the same peer must not duplicate");
    }

    #[test]
    fn list_is_most_recently_used_first() {
        let mut dir = PeerDirectory::new();
        dir.register(peer("a"));
        dir.register(peer("b"));
        dir.register(peer("c"));
        // Most recent registration (c) is first, oldest (a) last.
        assert_eq!(dir.list(), vec![peer("c"), peer("b"), peer("a")]);
    }

    #[test]
    fn touch_moves_a_peer_to_the_front() {
        let mut dir = PeerDirectory::new();
        dir.register(peer("a"));
        dir.register(peer("b"));
        dir.register(peer("c"));
        // Touching `a` (e.g. it just sent a message) makes it most-recently-used.
        dir.touch(&peer("a"));
        assert_eq!(dir.list(), vec![peer("a"), peer("c"), peer("b")]);
    }

    #[test]
    fn unregister_removes_a_peer() {
        let mut dir = PeerDirectory::new();
        dir.register(peer("a"));
        dir.register(peer("b"));
        assert!(dir.unregister(&peer("a")));
        assert!(!dir.contains(&peer("a")));
        assert_eq!(dir.list(), vec![peer("b")]);
        // Unregistering an absent peer is a no-op.
        assert!(!dir.unregister(&peer("a")));
    }

    #[test]
    fn addressable_excludes_self() {
        let mut dir = PeerDirectory::new();
        dir.register(peer("a"));
        dir.register(peer("b"));
        dir.register(peer("c"));
        let addressable = dir.addressable_by(&peer("b"));
        assert!(!addressable.contains(&peer("b")), "must not be able to address self");
        assert_eq!(addressable.len(), 2);
        // Still most-recently-used ordered: c registered after a.
        assert_eq!(addressable, vec![peer("c"), peer("a")]);
    }

    #[test]
    fn may_send_forbids_only_self() {
        let dir = PeerDirectory::new();
        assert!(!dir.may_send(&peer("a"), &peer("a")));
        assert!(dir.may_send(&peer("a"), &peer("b")));
    }

    #[test]
    fn may_send_allows_an_offline_target() {
        // A target need not be registered to be addressable: the broker queues to
        // its inbox. `may_send` therefore does not consult reachability.
        let dir = PeerDirectory::new();
        let online = peer("a");
        let offline = peer("b");
        assert!(dir.may_send(&online, &offline));
        assert!(!dir.contains(&offline));
    }

    #[test]
    fn distinct_nodes_are_distinct_peers() {
        let mut dir = PeerDirectory::new();
        let local = PeerId::new(AppId("a".into()), NodeId::local());
        let remote = PeerId::new(AppId("a".into()), NodeId("other".into()));
        dir.register(local.clone());
        dir.register(remote.clone());
        // Same App id, different node: two reachable peers.
        assert_eq!(dir.len(), 2);
        assert!(dir.may_send(&local, &remote));
    }
}
