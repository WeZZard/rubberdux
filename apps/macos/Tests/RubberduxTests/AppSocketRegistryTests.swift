import XCTest
@testable import Rubberdux

final class AppSocketRegistryTests: XCTestCase {

    private var registry: AppSocketRegistry!

    override func setUp() {
        super.setUp()
        // Use a URL that will not actually connect during unit tests
        registry = AppSocketRegistry(
            baseURL: URL(string: "http://localhost:0")!
        )
    }

    func testOpenCreatesSocketWithRefCountOne() {
        let socket = registry.open(appID: "app-1")
        XCTAssertEqual(socket.appID, "app-1")
        XCTAssertEqual(registry.refCount(for: "app-1"), 1)
    }

    func testOpenTwiceReturnsTheSameSocketWithRefCountTwo() {
        let first = registry.open(appID: "app-1")
        let second = registry.open(appID: "app-1")
        XCTAssertTrue(first === second)
        XCTAssertEqual(registry.refCount(for: "app-1"), 2)
    }

    func testCloseBelowZeroRemovesRecord() {
        registry.open(appID: "app-1")
        registry.close(appID: "app-1")
        XCTAssertEqual(registry.refCount(for: "app-1"), 0)
    }

    func testReleaseAtZeroAfterTwoOpensTwoCloses() {
        registry.open(appID: "app-1")
        registry.open(appID: "app-1")
        XCTAssertEqual(registry.refCount(for: "app-1"), 2)

        registry.close(appID: "app-1")
        XCTAssertEqual(registry.refCount(for: "app-1"), 1,
                       "First close should leave ref-count at 1")

        registry.close(appID: "app-1")
        XCTAssertEqual(registry.refCount(for: "app-1"), 0,
                       "Second close must release the socket (ref-count = 0)")
    }

    func testCloseUnknownAppIDIsNoop() {
        // Must not crash
        registry.close(appID: "unknown-app")
        XCTAssertEqual(registry.refCount(for: "unknown-app"), 0)
    }

    func testTwoDistinctAppsAreTrackedIndependently() {
        registry.open(appID: "app-a")
        registry.open(appID: "app-a")
        registry.open(appID: "app-b")

        XCTAssertEqual(registry.refCount(for: "app-a"), 2)
        XCTAssertEqual(registry.refCount(for: "app-b"), 1)

        registry.close(appID: "app-a")
        XCTAssertEqual(registry.refCount(for: "app-a"), 1)
        XCTAssertEqual(registry.refCount(for: "app-b"), 1)
    }
}
