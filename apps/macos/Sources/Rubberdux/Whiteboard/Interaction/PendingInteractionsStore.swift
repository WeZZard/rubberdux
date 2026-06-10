import Foundation

// MARK: - PendingInteractionsStore

/// The pure model of which interactions each App is currently awaiting. It is
/// the single source of truth for both the icon badge count and the pop-out
/// pending list, so the UI never derives counts ad hoc.
///
/// Interactions are keyed by `appID` then `requestId`, so a raise replaces any
/// prior raise of the same request (idempotent re-raise) and a resolve removes
/// exactly one. Ordering within an App is first-seen, so the pending list does
/// not reshuffle as items resolve.
///
/// This type holds no AppKit dependency so the badge-count and list-derivation
/// logic is unit-testable without UI. See
/// `docs/apps/macos/whiteboard-interaction.md` for the interaction-surface
/// design record.
struct PendingInteractionsStore: Equatable {

    /// Per-App ordered request ids, preserving first-seen order.
    private var order: [String: [String]] = [:]

    /// Per-App interactions keyed by request id.
    private var byRequest: [String: [String: AgentInteraction]] = [:]

    init() {}

    // MARK: - Mutation

    /// Record that `interaction` was raised. A re-raise of the same request id
    /// replaces the prior value in place without changing its position.
    mutating func raise(_ interaction: AgentInteraction) {
        let appID = interaction.appId
        let requestId = interaction.requestId
        if byRequest[appID]?[requestId] == nil {
            order[appID, default: []].append(requestId)
        }
        byRequest[appID, default: [:]][requestId] = interaction
    }

    /// Remove the interaction for `requestId` under `appID`, if present.
    mutating func resolve(appID: String, requestId: String) {
        guard byRequest[appID]?[requestId] != nil else { return }
        byRequest[appID]?.removeValue(forKey: requestId)
        order[appID]?.removeAll { $0 == requestId }
        if byRequest[appID]?.isEmpty == true {
            byRequest.removeValue(forKey: appID)
            order.removeValue(forKey: appID)
        }
    }

    /// Replace all pending interactions for `appID` with `interactions` (used to
    /// seed from a REST snapshot), preserving the supplied order.
    mutating func replace(appID: String, with interactions: [AgentInteraction]) {
        order[appID] = []
        byRequest[appID] = [:]
        for interaction in interactions {
            raise(interaction)
        }
        if interactions.isEmpty {
            order.removeValue(forKey: appID)
            byRequest.removeValue(forKey: appID)
        }
    }

    // MARK: - Derivation

    /// The number of interactions `appID` is awaiting. Drives the icon badge.
    func badgeCount(forAppID appID: String) -> Int {
        order[appID]?.count ?? 0
    }

    /// The interactions `appID` is awaiting, in first-seen order. Drives the
    /// pop-out pending list.
    func interactions(forAppID appID: String) -> [AgentInteraction] {
        guard let ids = order[appID], let map = byRequest[appID] else { return [] }
        return ids.compactMap { map[$0] }
    }

    /// Whether `appID` currently awaits any interaction.
    func hasPending(forAppID appID: String) -> Bool {
        badgeCount(forAppID: appID) > 0
    }
}
