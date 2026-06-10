import XCTest
@testable import Rubberdux

final class BoardAppStoreTests: XCTestCase {

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

    // MARK: - Snapshot

    func testApplySnapshotPopulatesStore() {
        let store = BoardAppStore()
        let change = store.applySnapshot([makeApp(id: "a"), makeApp(id: "b")])
        XCTAssertEqual(change, .replacedAll)
        XCTAssertEqual(store.apps.map(\.id), ["a", "b"])
    }

    func testApplySameSnapshotTwiceIsIdempotent() {
        let store = BoardAppStore()
        let apps = [makeApp(id: "a"), makeApp(id: "b")]
        XCTAssertEqual(store.applySnapshot(apps), .replacedAll)
        XCTAssertEqual(store.applySnapshot(apps), .unchanged)
        XCTAssertEqual(store.apps.count, 2)
    }

    // MARK: - Event idempotency

    func testAppCreatedEventIsIdempotent() {
        let store = BoardAppStore()
        let app = makeApp(id: "a")
        XCTAssertEqual(store.apply(.appCreated(app: app)), .upserted(app))
        XCTAssertEqual(store.apply(.appCreated(app: app)), .unchanged)
        XCTAssertEqual(store.apps.count, 1)
    }

    func testArchivedEventRemovesAndIsIdempotent() {
        let store = BoardAppStore()
        store.applySnapshot([makeApp(id: "a")])
        XCTAssertEqual(store.apply(.archived(id: "a")), .removed(id: "a"))
        XCTAssertEqual(store.apply(.archived(id: "a")), .unchanged)
        XCTAssertNil(store.app(id: "a"))
    }

    func testUpsertWithEqualAppIsUnchanged() {
        let store = BoardAppStore()
        let app = makeApp(id: "a")
        XCTAssertEqual(store.upsert(app), .upserted(app))
        XCTAssertEqual(store.upsert(app), .unchanged)
    }

    func testUpsertWithMovedAppReportsUpsert() {
        let store = BoardAppStore()
        store.upsert(makeApp(id: "a", row: 0, column: 0))
        let moved = makeApp(id: "a", row: 3, column: 4)
        XCTAssertEqual(store.upsert(moved), .upserted(moved))
        XCTAssertEqual(store.app(id: "a")?.position, BoardPosition(row: 3, column: 4))
    }

    // MARK: - Optimistic placeholder collapse

    func testReconcileCollapsesPlaceholderIntoServerApp() {
        let store = BoardAppStore()
        let placeholderID = BoardAppStore.placeholderID()
        let placeholder = makeApp(id: placeholderID, title: "Optimistic")
        XCTAssertEqual(store.insertPlaceholder(placeholder), .upserted(placeholder))

        let server = makeApp(id: "server-1", title: "Optimistic")
        let change = store.reconcileCreate(placeholderID: placeholderID, with: server)
        XCTAssertEqual(change, .upserted(server))

        // The placeholder is gone and only the server id survives — no duplicate.
        XCTAssertNil(store.app(id: placeholderID))
        XCTAssertEqual(store.app(id: "server-1"), server)
        XCTAssertEqual(store.apps.count, 1)
    }

    func testReconcileIsIdempotent() {
        let store = BoardAppStore()
        let placeholderID = BoardAppStore.placeholderID()
        store.insertPlaceholder(makeApp(id: placeholderID))
        let server = makeApp(id: "server-1")
        XCTAssertEqual(store.reconcileCreate(placeholderID: placeholderID, with: server), .upserted(server))
        XCTAssertEqual(store.reconcileCreate(placeholderID: placeholderID, with: server), .unchanged)
        XCTAssertEqual(store.apps.count, 1)
    }

    func testBoardEventForReconciledIDCollapsesPlaceholder() {
        // A board `app_created` for the server id arrives after the placeholder
        // was already mapped to that server id; it must not create a duplicate.
        let store = BoardAppStore()
        let placeholderID = BoardAppStore.placeholderID()
        store.insertPlaceholder(makeApp(id: placeholderID))
        let server = makeApp(id: "server-1")
        store.reconcileCreate(placeholderID: placeholderID, with: server)

        // Replaying the placeholder mapping path: upserting the server app again
        // by id stays at one entry.
        XCTAssertEqual(store.apply(.appCreated(app: server)), .unchanged)
        XCTAssertEqual(store.apps.count, 1)
        XCTAssertNil(store.app(id: placeholderID))
    }

    func testReconcileWhenBoardEventAlreadyInsertedServerApp() {
        // The board event inserts the real app by server id before reconcile
        // runs; reconcile then drops the placeholder, leaving a single entry.
        let store = BoardAppStore()
        let placeholderID = BoardAppStore.placeholderID()
        store.insertPlaceholder(makeApp(id: placeholderID))
        let server = makeApp(id: "server-1")
        store.apply(.appCreated(app: server))
        XCTAssertEqual(store.apps.count, 2) // placeholder + server, transient

        let change = store.reconcileCreate(placeholderID: placeholderID, with: server)
        XCTAssertEqual(change, .upserted(server))
        XCTAssertNil(store.app(id: placeholderID))
        XCTAssertEqual(store.apps.count, 1)
    }
}
