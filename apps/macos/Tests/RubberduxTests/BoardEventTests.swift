import XCTest
@testable import Rubberdux

final class BoardEventTests: XCTestCase {

    private func roundTrip<T: Codable & Equatable>(_ value: T) throws -> T {
        let data = try JSONEncoder().encode(value)
        return try JSONDecoder().decode(T.self, from: data)
    }

    private func makeApp() -> App {
        App(
            id: "app-1",
            title: "Test App",
            icon: Icon(symbol: "star", color: "#0000FF"),
            position: BoardPosition(row: 1, column: 2),
            status: .active,
            summary: "A test",
            userLocked: false,
            lastActive: "2024-01-01T00:00:00Z"
        )
    }

    func testCreatedRoundTrip() throws {
        let event = BoardEvent.created(app: makeApp())
        XCTAssertEqual(try roundTrip(event), event)
    }

    func testCreatedKindTag() throws {
        let event = BoardEvent.created(app: makeApp())
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(event)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "created")
    }

    func testStatusChangedRoundTrip() throws {
        let event = BoardEvent.statusChanged(id: "app-1", status: .tombstoned)
        XCTAssertEqual(try roundTrip(event), event)
    }

    func testStatusChangedKindTag() throws {
        let event = BoardEvent.statusChanged(id: "app-1", status: .active)
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(event)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "status_changed")
    }

    func testMovedRoundTrip() throws {
        let event = BoardEvent.moved(id: "app-1", position: BoardPosition(row: 3, column: 4))
        XCTAssertEqual(try roundTrip(event), event)
    }

    func testMovedKindTag() throws {
        let event = BoardEvent.moved(id: "app-1", position: BoardPosition(row: 0, column: 0))
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(event)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "moved")
    }

    func testArchivedRoundTrip() throws {
        let event = BoardEvent.archived(id: "app-1")
        XCTAssertEqual(try roundTrip(event), event)
    }

    func testArchivedKindTag() throws {
        let event = BoardEvent.archived(id: "app-1")
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(event)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "archived")
    }
}
