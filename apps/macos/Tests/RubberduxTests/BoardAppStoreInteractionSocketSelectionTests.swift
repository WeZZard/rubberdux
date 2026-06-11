import XCTest
@testable import Rubberdux

/// Verifies the pure interactions-socket id selector that drives
/// `WhiteboardViewController.reconcileInteractionSockets()`. The selector must
/// never return an optimistic placeholder id (the backend has no worker for it),
/// and must return the server id once the optimistic→server reconciliation has
/// swapped the placeholder — including the `.unchanged` reconciliation path.
final class BoardAppStoreInteractionSocketSelectionTests: XCTestCase {

    // MARK: - Fixtures

    private func makeApp(
        id: String,
        row: Int = 0,
        column: Int = 0,
        title: String = "Task",
        status: AppStatus = .active
    ) -> App {
        App(
            id: id,
            title: title,
            icon: Icon(symbol: "star", color: "#FF0000"),
            position: BoardPosition(row: row, column: column),
            status: status,
            summary: title,
            userLocked: false,
            lastActive: "2024-01-01T00:00:00Z"
        )
    }

    // MARK: - Placeholder exclusion

    func testSelectorExcludesOptimisticPlaceholderID() {
        let placeholderID = BoardAppStore.placeholderID()
        let apps = [
            makeApp(id: placeholderID),
            makeApp(id: "server-1"),
        ]
        let ids = BoardAppStore.interactionSocketIDs(for: apps)
        XCTAssertFalse(ids.contains(placeholderID))
        XCTAssertEqual(ids, ["server-1"])
    }

    func testSelectorReturnsNoOptimisticIDForAllPlaceholderBoard() {
        let apps = [
            makeApp(id: BoardAppStore.placeholderID()),
            makeApp(id: BoardAppStore.placeholderID()),
        ]
        XCTAssertTrue(BoardAppStore.interactionSocketIDs(for: apps).isEmpty)
        XCTAssertTrue(
            BoardAppStore.interactionSocketIDs(for: apps)
                .allSatisfy { !BoardAppStore.isPlaceholderID($0) }
        )
    }

    func testSelectorExcludesInactiveApps() {
        let apps = [
            makeApp(id: "server-1", status: .active),
            makeApp(id: "server-2", status: .tombstoned),
        ]
        XCTAssertEqual(BoardAppStore.interactionSocketIDs(for: apps), ["server-1"])
    }

    // MARK: - Optimistic → server reconciliation

    func testServerIDSelectedAfterReconcileSwap() {
        let store = BoardAppStore()
        let placeholderID = BoardAppStore.placeholderID()
        store.insertPlaceholder(makeApp(id: placeholderID))

        // Before reconciliation: the only App is the placeholder, so the selector
        // returns nothing — no socket would be opened for the optimistic id.
        XCTAssertTrue(BoardAppStore.interactionSocketIDs(for: store.apps).isEmpty)

        let server = makeApp(id: "server-1")
        XCTAssertEqual(
            store.reconcileCreate(placeholderID: placeholderID, with: server),
            .upserted(server)
        )

        // After the swap the server id IS selected and the placeholder is gone.
        let ids = BoardAppStore.interactionSocketIDs(for: store.apps)
        XCTAssertEqual(ids, ["server-1"])
        XCTAssertFalse(ids.contains(placeholderID))
    }

    func testServerIDSelectedWhenReconcileResolvesUnchanged() {
        // A board `app_created` event inserts the server App before reconcile
        // runs; reconcile then drops the placeholder. When the reconciled App
        // equals the already-inserted one and the placeholder is gone, reconcile
        // reports `.unchanged`. Even so, the server id must be selectable so the
        // server-id interactions socket opens on the `.unchanged` reconciliation.
        let store = BoardAppStore()
        let placeholderID = BoardAppStore.placeholderID()
        store.insertPlaceholder(makeApp(id: placeholderID))
        let server = makeApp(id: "server-1")

        // First reconcile collapses the placeholder into the server id.
        XCTAssertEqual(
            store.reconcileCreate(placeholderID: placeholderID, with: server),
            .upserted(server)
        )
        // Replaying reconcile now reports `.unchanged` (placeholder already gone,
        // server App already equal).
        XCTAssertEqual(
            store.reconcileCreate(placeholderID: placeholderID, with: server),
            .unchanged
        )

        // The server id remains selected through the `.unchanged` resolution.
        let ids = BoardAppStore.interactionSocketIDs(for: store.apps)
        XCTAssertEqual(ids, ["server-1"])
        XCTAssertFalse(ids.contains(placeholderID))
    }
}
