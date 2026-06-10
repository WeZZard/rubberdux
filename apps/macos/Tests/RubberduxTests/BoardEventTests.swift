import XCTest
@testable import Rubberdux

/// Decode tests for `BoardEvent` against the real backend-shaped frames the
/// gateway emits from `BoardWsMessage` in `src/gateway/apps_stream.rs`. These
/// assert on the actual wire JSON (tag field `type`, snake_case keys), not a
/// Swift-internal round-trip, so a drift from the backend surface fails here.
final class BoardEventTests: XCTestCase {

    private func decode(_ json: String) throws -> BoardEvent {
        try JSONDecoder().decode(BoardEvent.self, from: Data(json.utf8))
    }

    func testDecodesAppCreatedFromBackendFrame() throws {
        let json = """
        {
            "type": "app_created",
            "app": {
                "id": "2024-01-01-00-00-00-UTC",
                "title": "Plan offsite",
                "icon": { "symbol": "calendar", "color": "#FF6B6B" },
                "position": { "row": 1, "column": 2 },
                "status": "active",
                "summary": "Planning an offsite",
                "user_locked": false,
                "last_active": "2024-01-01T00:00:00Z"
            }
        }
        """
        guard case .appCreated(let app) = try decode(json) else {
            return XCTFail("expected .appCreated")
        }
        XCTAssertEqual(app.id, "2024-01-01-00-00-00-UTC")
        XCTAssertEqual(app.title, "Plan offsite")
        XCTAssertEqual(app.icon.symbol, "calendar")
        XCTAssertEqual(app.status, .active)
    }

    func testDecodesUpdatedFromBackendFrame() throws {
        let json = #"{ "type": "updated", "id": "app-y" }"#
        guard case .updated(let id) = try decode(json) else {
            return XCTFail("expected .updated")
        }
        XCTAssertEqual(id, "app-y")
    }

    func testDecodesArchivedFromBackendFrame() throws {
        let json = #"{ "type": "archived", "id": "app-z" }"#
        guard case .archived(let id) = try decode(json) else {
            return XCTFail("expected .archived")
        }
        XCTAssertEqual(id, "app-z")
    }

    func testDecodesBadgeFromBackendFrame() throws {
        let json = #"{ "type": "badge", "app_id": "app-x", "count": 2 }"#
        guard case .badge(let appId, let count) = try decode(json) else {
            return XCTFail("expected .badge")
        }
        XCTAssertEqual(appId, "app-x")
        XCTAssertEqual(count, 2)
    }

    func testRejectsUnknownType() {
        let json = #"{ "type": "moved", "id": "app-1" }"#
        XCTAssertThrowsError(try decode(json))
    }
}
