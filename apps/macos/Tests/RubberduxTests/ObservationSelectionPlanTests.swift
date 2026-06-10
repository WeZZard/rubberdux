import XCTest
@testable import Rubberdux

final class ObservationSelectionPlanTests: XCTestCase {

    func testEmptySelectionIsNotVisible() {
        let plan = ObservationSelectionPlan(selection: [], order: ["a", "b"])
        XCTAssertTrue(plan.appIDs.isEmpty)
        XCTAssertFalse(plan.isVisible)
    }

    func testNonEmptySelectionIsVisible() {
        let plan = ObservationSelectionPlan(selection: ["a"], order: ["a", "b"])
        XCTAssertEqual(plan.appIDs, ["a"])
        XCTAssertTrue(plan.isVisible)
    }

    func testOrderFollowsReferenceOrderNotSetIteration() {
        let plan = ObservationSelectionPlan(
            selection: ["c", "a", "b"],
            order: ["a", "b", "c", "d"]
        )
        XCTAssertEqual(plan.appIDs, ["a", "b", "c"])
    }

    func testSelectionAbsentFromOrderIsAppendedSorted() {
        let plan = ObservationSelectionPlan(
            selection: ["z", "a", "m"],
            order: ["a"]
        )
        // "a" is in order; "m" and "z" are not, appended sorted.
        XCTAssertEqual(plan.appIDs, ["a", "m", "z"])
    }

    func testNoDuplicatesEvenIfOrderRepeats() {
        let plan = ObservationSelectionPlan(
            selection: ["a"],
            order: ["a", "a", "a"]
        )
        XCTAssertEqual(plan.appIDs, ["a"])
    }

    func testEqualPlansForSameSelectionAndOrder() {
        let first = ObservationSelectionPlan(selection: ["a", "b"], order: ["a", "b"])
        let second = ObservationSelectionPlan(selection: ["b", "a"], order: ["a", "b"])
        XCTAssertEqual(first, second)
    }
}
