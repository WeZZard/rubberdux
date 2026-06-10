import XCTest
@testable import Rubberdux

final class PendingInteractionsStoreTests: XCTestCase {

    private func approval(_ requestId: String, app: String = "a") -> AgentInteraction {
        .approval(requestId: requestId, appId: app, flavor: .permission, prompt: "p")
    }

    func testEmptyStoreHasZeroBadgeAndNoPending() {
        let store = PendingInteractionsStore()
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 0)
        XCTAssertFalse(store.hasPending(forAppID: "a"))
        XCTAssertTrue(store.interactions(forAppID: "a").isEmpty)
    }

    func testRaiseIncrementsBadge() {
        var store = PendingInteractionsStore()
        store.raise(approval("r1"))
        store.raise(approval("r2"))
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 2)
        XCTAssertTrue(store.hasPending(forAppID: "a"))
    }

    func testReRaiseSameRequestIsIdempotent() {
        var store = PendingInteractionsStore()
        store.raise(approval("r1"))
        store.raise(approval("r1"))
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 1)
    }

    func testResolveDecrementsBadgeAndRemovesFromList() {
        var store = PendingInteractionsStore()
        store.raise(approval("r1"))
        store.raise(approval("r2"))
        store.resolve(appID: "a", requestId: "r1")
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 1)
        XCTAssertEqual(store.interactions(forAppID: "a").map(\.requestId), ["r2"])
    }

    func testResolveUnknownRequestIsNoOp() {
        var store = PendingInteractionsStore()
        store.raise(approval("r1"))
        store.resolve(appID: "a", requestId: "nope")
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 1)
    }

    func testListPreservesFirstSeenOrder() {
        var store = PendingInteractionsStore()
        store.raise(approval("r3"))
        store.raise(approval("r1"))
        store.raise(approval("r2"))
        XCTAssertEqual(store.interactions(forAppID: "a").map(\.requestId), ["r3", "r1", "r2"])
    }

    func testPerAppIsolation() {
        var store = PendingInteractionsStore()
        store.raise(approval("r1", app: "a"))
        store.raise(approval("r1", app: "b"))
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 1)
        XCTAssertEqual(store.badgeCount(forAppID: "b"), 1)
        store.resolve(appID: "a", requestId: "r1")
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 0)
        XCTAssertEqual(store.badgeCount(forAppID: "b"), 1)
    }

    func testReplaceSeedsFromSnapshot() {
        var store = PendingInteractionsStore()
        store.raise(approval("stale"))
        store.replace(appID: "a", with: [approval("r1"), approval("r2")])
        XCTAssertEqual(store.interactions(forAppID: "a").map(\.requestId), ["r1", "r2"])
    }

    func testReplaceWithEmptyClears() {
        var store = PendingInteractionsStore()
        store.raise(approval("r1"))
        store.replace(appID: "a", with: [])
        XCTAssertEqual(store.badgeCount(forAppID: "a"), 0)
        XCTAssertFalse(store.hasPending(forAppID: "a"))
    }
}
