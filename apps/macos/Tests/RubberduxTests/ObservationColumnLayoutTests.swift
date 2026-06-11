import XCTest
@testable import Rubberdux

/// Covers the pure column-layout decision that `ObservationPanelViewController`
/// uses to add, remove, and reorder its per-App observation columns, plus the
/// mapping of pending interactions to the column that should render them.
final class ObservationColumnLayoutTests: XCTestCase {

    // MARK: - Ordered column ids

    func testEmptySelectionYieldsNoColumns() {
        let ids = ObservationSelectionPlan.orderedColumnIDs(selection: [], order: ["a", "b"])
        XCTAssertTrue(ids.isEmpty)
        let plan = ObservationSelectionPlan(selection: [], order: ["a", "b"])
        XCTAssertFalse(plan.isVisible)
    }

    func testOneColumnPerSelectedAppInBoardOrder() {
        let ids = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["c", "a", "b"],
            order: ["a", "b", "c", "d"]
        )
        XCTAssertEqual(ids, ["a", "b", "c"])
    }

    func testAddingASelectionAppendsAColumnInBoardOrder() {
        let before = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["a"],
            order: ["a", "b", "c"]
        )
        XCTAssertEqual(before, ["a"])
        let after = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["a", "c"],
            order: ["a", "b", "c"]
        )
        XCTAssertEqual(after, ["a", "c"])
    }

    func testRemovingASelectionDropsItsColumn() {
        let after = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["a", "c"],
            order: ["a", "b", "c"]
        )
        XCTAssertEqual(after, ["a", "c"])
        XCTAssertFalse(after.contains("b"))
    }

    func testColumnOrderTracksBoardOrderNotSelectionInsertionOrder() {
        // The selection set is unordered; the column order is the board order,
        // so reordering apps on the board reorders the columns deterministically.
        let ids = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["b", "a"],
            order: ["a", "b"]
        )
        let reordered = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["a", "b"],
            order: ["b", "a"]
        )
        XCTAssertEqual(ids, ["a", "b"])
        XCTAssertEqual(reordered, ["b", "a"])
    }

    func testSelectionAbsentFromBoardOrderIsAppendedSorted() {
        let ids = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["z", "a", "m"],
            order: ["a"]
        )
        XCTAssertEqual(ids, ["a", "m", "z"])
    }

    func testNoDuplicateColumnsWhenBoardOrderRepeats() {
        let ids = ObservationSelectionPlan.orderedColumnIDs(
            selection: ["a"],
            order: ["a", "a", "a"]
        )
        XCTAssertEqual(ids, ["a"])
    }

    // MARK: - Interactions map to the matching column

    func testInteractionsAreScopedToTheMatchingColumn() {
        var store = PendingInteractionsStore()
        store.raise(interaction(appID: "a", requestID: "r1"))
        store.raise(interaction(appID: "a", requestID: "r2"))
        store.raise(interaction(appID: "b", requestID: "r3"))

        // The panel feeds each column `interactions(forAppID:)`; only that App's
        // interactions reach its column.
        let columnA = store.interactions(forAppID: "a").map(\.requestId)
        let columnB = store.interactions(forAppID: "b").map(\.requestId)

        XCTAssertEqual(columnA, ["r1", "r2"])
        XCTAssertEqual(columnB, ["r3"])
        XCTAssertTrue(store.interactions(forAppID: "c").isEmpty)
    }

    func testResolvingAnInteractionRemovesItFromOnlyItsColumn() {
        var store = PendingInteractionsStore()
        store.raise(interaction(appID: "a", requestID: "r1"))
        store.raise(interaction(appID: "b", requestID: "r2"))

        store.resolve(appID: "a", requestId: "r1")

        XCTAssertTrue(store.interactions(forAppID: "a").isEmpty)
        XCTAssertEqual(store.interactions(forAppID: "b").map(\.requestId), ["r2"])
    }

    // MARK: - Helpers

    private func interaction(appID: String, requestID: String) -> AgentInteraction {
        .approval(requestId: requestID, appId: appID, flavor: .permission, prompt: "go?")
    }
}
