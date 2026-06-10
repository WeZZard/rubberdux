import Foundation

// MARK: - BoardAppStoreChange

/// The minimal description of how the store's contents changed after applying a
/// snapshot or a board event. The view controller uses this to drive the board
/// view incrementally instead of rebuilding every icon on every change.
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
enum BoardAppStoreChange: Equatable {
    /// The whole set was replaced (e.g. after a `GET /apps` snapshot). The
    /// caller should resync the board view to `apps`.
    case replacedAll
    /// A single App was inserted or updated in place.
    case upserted(App)
    /// A single App was removed (archived or tombstoned).
    case removed(id: String)
    /// Nothing observable changed; the input was a duplicate of current state.
    case unchanged
}

// MARK: - BoardAppStore

/// An in-memory, id-keyed store of the board's `App`s. It is the single source
/// of truth the whiteboard view controller renders from. The store applies a
/// `GET /apps` snapshot and individual `BoardEvent`s idempotently, so replaying
/// the same snapshot or event leaves the contents unchanged.
///
/// It also models the optimistic-create flow: a placeholder `App` (carrying a
/// client-minted temporary id) is inserted immediately on submit, then collapsed
/// into the reconciled server `App` once `POST /apps` returns — the placeholder
/// is removed and replaced by the real id in a single step, so no duplicate icon
/// survives reconciliation.
///
/// The store holds no view dependency; it returns a `BoardAppStoreChange` the
/// caller maps onto `BoardView`. It is not thread-safe and is expected to be
/// driven from the main thread (AppKit event loop).
final class BoardAppStore {

    // MARK: - State

    /// Apps keyed by id. Includes optimistic placeholders until they reconcile.
    private(set) var appsByID: [String: App] = [:]

    /// Maps a placeholder's temporary id to the reconciled server id, once
    /// known, so a late board event addressed to either id resolves to the same
    /// entry. The mapping is dropped once both sides agree on the server id.
    private var placeholderToServerID: [String: String] = [:]

    // MARK: - Snapshot

    /// All current apps, ordered by id for a deterministic enumeration. Callers
    /// that need board ordering derive it from position, not from this order.
    var apps: [App] {
        appsByID.values.sorted { $0.id < $1.id }
    }

    /// The App for `id`, if present.
    func app(id: String) -> App? {
        appsByID[id]
    }

    // MARK: - Snapshot application

    /// Replace the store's contents with a `GET /apps` snapshot. Applying the
    /// same snapshot twice is idempotent: identical contents report `.unchanged`.
    /// Outstanding optimistic placeholders that the snapshot does not mention are
    /// preserved, because a placeholder may still be in flight.
    @discardableResult
    func applySnapshot(_ apps: [App]) -> BoardAppStoreChange {
        var next: [String: App] = [:]
        for app in apps {
            next[app.id] = app
        }
        // Preserve in-flight placeholders the snapshot has not yet reconciled.
        for (id, app) in appsByID where placeholderToServerID[id] != nil || isPlaceholder(id) {
            if next[id] == nil, placeholderToServerID[id] == nil {
                next[id] = app
            }
        }
        if next == appsByID {
            return .unchanged
        }
        appsByID = next
        return .replacedAll
    }

    // MARK: - Optimistic create

    /// Insert an optimistic placeholder `App` and return its temporary id. The
    /// placeholder renders immediately; `reconcileCreate(placeholderID:with:)`
    /// later collapses it into the server's reconciled `App`.
    @discardableResult
    func insertPlaceholder(_ placeholder: App) -> BoardAppStoreChange {
        guard appsByID[placeholder.id] == nil else { return .unchanged }
        appsByID[placeholder.id] = placeholder
        return .upserted(placeholder)
    }

    /// Mint a temporary id for an optimistic placeholder. The prefix marks it as
    /// client-local so it is never confused with a server id.
    static func placeholderID() -> String {
        "optimistic-\(UUID().uuidString)"
    }

    private func isPlaceholder(_ id: String) -> Bool {
        id.hasPrefix("optimistic-")
    }

    /// Collapse the optimistic placeholder identified by `placeholderID` into the
    /// reconciled server `app`, keyed by the server's id. If the placeholder is
    /// gone (e.g. a board `app_created` event already inserted the real app), the
    /// reconciled app is upserted by its server id and any duplicate placeholder
    /// removed. Applying this twice with the same reconciled app is idempotent.
    @discardableResult
    func reconcileCreate(placeholderID: String, with app: App) -> BoardAppStoreChange {
        placeholderToServerID[placeholderID] = app.id
        let hadPlaceholder = appsByID.removeValue(forKey: placeholderID) != nil
        let existing = appsByID[app.id]
        appsByID[app.id] = app
        if existing == app, !hadPlaceholder {
            return .unchanged
        }
        return .upserted(app)
    }

    // MARK: - Board event application

    /// Apply a `BoardEvent` from the board WebSocket stream idempotently. For
    /// `.updated` the event carries only an id, so the caller is expected to have
    /// refetched the App and pass it via `upsert(_:)`; this method handles the
    /// self-contained events (`app_created`, `archived`, `badge`). The returned
    /// change tells the caller how to update the board view. `.badge` reports
    /// `.unchanged` here because badge counts are not part of the App model; the
    /// caller applies badges to the board view directly from the event.
    @discardableResult
    func apply(_ event: BoardEvent) -> BoardAppStoreChange {
        switch event {
        case let .appCreated(app):
            return upsert(app)
        case .updated:
            // The wire event carries only an id; the caller refetches and
            // upserts. Nothing to apply here from the event alone.
            return .unchanged
        case let .archived(id):
            return remove(id: id)
        case .badge:
            return .unchanged
        }
    }

    /// Insert or update `app` by its id. Returns `.unchanged` if the stored App
    /// already equals `app`, so a replayed `app_created` or a refetched
    /// `.updated` is idempotent. A matching optimistic placeholder is collapsed
    /// so the create flow never leaves a duplicate icon behind.
    @discardableResult
    func upsert(_ app: App) -> BoardAppStoreChange {
        for (placeholderID, serverID) in placeholderToServerID where serverID == app.id {
            if appsByID.removeValue(forKey: placeholderID) != nil {
                appsByID[app.id] = app
                return .upserted(app)
            }
        }
        if appsByID[app.id] == app {
            return .unchanged
        }
        appsByID[app.id] = app
        return .upserted(app)
    }

    /// Remove the App for `id`. Returns `.unchanged` if no such App exists, so a
    /// replayed `archived` event is idempotent.
    @discardableResult
    func remove(id: String) -> BoardAppStoreChange {
        guard appsByID.removeValue(forKey: id) != nil else {
            return .unchanged
        }
        placeholderToServerID = placeholderToServerID.filter { $0.value != id }
        return .removed(id: id)
    }
}
