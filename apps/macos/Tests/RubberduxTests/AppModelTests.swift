import XCTest
@testable import Rubberdux

final class AppModelTests: XCTestCase {

    // MARK: - App round-trip

    func testAppDecodesFromJSON() throws {
        let json = """
        {
            "id": "2024-01-01-00-00-00-UTC",
            "title": "Plan offsite",
            "icon": { "symbol": "calendar", "color": "#FF6B6B" },
            "position": { "row": 1, "column": 2 },
            "status": "active",
            "summary": "Planning an offsite",
            "user_locked": false,
            "last_active": "2024-01-01T00:00:00Z"
        }
        """.data(using: .utf8)!

        let app = try JSONDecoder().decode(App.self, from: json)
        XCTAssertEqual(app.id, "2024-01-01-00-00-00-UTC")
        XCTAssertEqual(app.title, "Plan offsite")
        XCTAssertEqual(app.icon.symbol, "calendar")
        XCTAssertEqual(app.icon.color, "#FF6B6B")
        XCTAssertEqual(app.position.row, 1)
        XCTAssertEqual(app.position.column, 2)
        XCTAssertEqual(app.status, .active)
        XCTAssertFalse(app.userLocked)
    }

    func testAppRoundTrip() throws {
        let app = App(
            id: "test-id",
            title: "Test",
            icon: Icon(symbol: "star", color: "#0000FF"),
            position: BoardPosition(row: 0, column: 0),
            status: .tombstoned,
            summary: "A test app",
            userLocked: true,
            lastActive: "2024-06-01T12:00:00Z"
        )
        let data = try JSONEncoder().encode(app)
        let decoded = try JSONDecoder().decode(App.self, from: data)
        XCTAssertEqual(app, decoded)
    }

    func testBoardPositionRoundTrip() throws {
        let position = BoardPosition(row: 3, column: 7)
        let data = try JSONEncoder().encode(position)
        let decoded = try JSONDecoder().decode(BoardPosition.self, from: data)
        XCTAssertEqual(position, decoded)
    }
}
