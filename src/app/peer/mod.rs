//! The decentralized peer-messaging primitive for the App network.
//!
//! Every App is a node that other Apps can address and talk to directly. This
//! module models that agent-to-agent network with three pieces:
//!
//! - [`PeerId`] — a node-addressable identity (an App id plus the node it lives
//!   on). The same identity works for a local App and, later, an App on another
//!   machine, so the broker can federate across machines without changing the
//!   addressing model.
//! - [`directory::PeerDirectory`] — a *dynamic* directory of the Apps that are
//!   reachable right now. An App registers when it becomes active and refreshes
//!   its position on every message (most-recently-used ordering); any App can ask
//!   "who can I talk to right now?".
//! - [`mailbox::Mailbox`] — the per-App inbox (`inbox.jsonl`) that holds messages
//!   addressed to an App whose worker is not currently running. Whether an App is
//!   tombstoned or human-archived is orthogonal to whether it is addressable: a
//!   message to an offline App is queued and drained when the App next comes up.
//!
//! The host is a directory + relay only — a switch, not an orchestrator: it
//! resolves a [`PeerId`] to a connection and forwards opaque message envelopes,
//! making no routing or coordination decisions. The rationale, the
//! local-and-remote-capable design, and why archiving does not gate
//! addressability are recorded in `docs/app/peer/decentralized-messaging.md`.

pub mod directory;
pub mod mailbox;

use serde::{Deserialize, Serialize};

use crate::app::AppId;

/// The node an App lives on. A node-addressable [`PeerId`] pairs an App id with
/// the identity of the node hosting it, so the broker resolves a target the same
/// way whether the App is on this machine or — once the broker federates over the
/// general TCP transport — on another. The string is opaque to the broker: it is
/// only matched for equality and used to pick the connection to a node.
///
/// [`NodeId::LOCAL`] names this machine. A federated deployment assigns each node
/// a stable id; until then every App is local, so the directory and broker behave
/// identically to a single-machine setup without baking that assumption in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// The conventional id of the local node — the machine this host runs on.
    pub const LOCAL: &'static str = "local";

    /// The id naming this machine, used for every App until cross-machine
    /// federation assigns nodes distinct identities.
    pub fn local() -> Self {
        Self(Self::LOCAL.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A node-addressable identity for one App on the peer network: which App
/// ([`AppId`]) and which node it lives on ([`NodeId`]). The broker resolves a
/// `PeerId` to a connection — locally today, across machines once it federates —
/// so the same identity is the unit of addressing in both cases.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId {
    pub app_id: AppId,
    pub node_id: NodeId,
}

impl PeerId {
    /// Construct a peer id for an App on a given node.
    pub fn new(app_id: AppId, node_id: NodeId) -> Self {
        Self { app_id, node_id }
    }

    /// Construct a peer id for an App on the local node, the common case until
    /// the broker federates across machines.
    pub fn local(app_id: AppId) -> Self {
        Self {
            app_id,
            node_id: NodeId::local(),
        }
    }

    /// Whether this peer lives on the local node, so the broker can deliver it
    /// directly rather than forwarding to another node.
    pub fn is_local(&self) -> bool {
        self.node_id.as_str() == NodeId::LOCAL
    }
}

impl std::fmt::Display for PeerId {
    /// Render as `node/app` so a log line is unambiguous about which machine a
    /// target lives on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.node_id, self.app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_peer_is_on_the_local_node() {
        let id = PeerId::local(AppId("2026-06-10-00-00-00-UTC".into()));
        assert!(id.is_local());
        assert_eq!(id.node_id, NodeId::local());
    }

    #[test]
    fn a_remote_node_is_not_local() {
        let id = PeerId::new(
            AppId("2026-06-10-00-00-00-UTC".into()),
            NodeId("other-machine".into()),
        );
        assert!(!id.is_local());
    }

    #[test]
    fn peer_id_round_trips_through_json() {
        let id = PeerId::new(
            AppId("2026-06-10-00-00-00-UTC".into()),
            NodeId("node-7".into()),
        );
        let restored: PeerId = serde_json::from_str(&serde_json::to_string(&id).unwrap()).unwrap();
        assert_eq!(restored, id);
    }

    #[test]
    fn peer_id_displays_node_then_app() {
        let id = PeerId::new(AppId("app-1".into()), NodeId("node-2".into()));
        assert_eq!(id.to_string(), "node-2/app-1");
    }
}
