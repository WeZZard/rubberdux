import AppKit
import Combine
import Network
import XCTest
@testable import Rubberdux

// MARK: - SpyAXView

/// An `NSView` standing in for a real surface root whose Accessibility actions are
/// captured rather than performed. `AccessibilitySurfaceDriveTarget` flattens its
/// root's AX subtree depth-first with the ROOT at index 0, so addressing element
/// `0` resolves to this view itself; overriding the two AX entry points the target
/// calls lets a test prove which AX action a `SurfaceOp` fired, with what argument,
/// without a live UI.
final class SpyAXView: NSView {
    /// Every argument passed to `setAccessibilityValue(_:)`, in call order.
    private(set) var setValueArguments: [Any?] = []
    /// How many times `accessibilityPerformPress()` fired.
    private(set) var pressCount = 0

    override func setAccessibilityValue(_ accessibilityValue: Any?) {
        setValueArguments.append(accessibilityValue)
    }

    override func accessibilityPerformPress() -> Bool {
        pressCount += 1
        return true
    }
}

// MARK: - RecordingDriveTarget

/// A `SurfaceDriveTarget` that records which seam method the controller dispatched
/// each op to (and its arguments) without touching any UI, so the controller's
/// `Vec`-order application, version bookkeeping, and optimistic-concurrency
/// rejection can be exercised in isolation.
final class RecordingDriveTarget: SurfaceDriveTarget {
    enum Call: Equatable {
        case setValue(SurfaceId, ElementId, JSONValue)
        case click(SurfaceId, ElementId, SurfacePoint?)
        case navigate(SurfaceId, Route)
        case render(SurfaceId, ComponentSpec)
    }

    private(set) var calls: [Call] = []
    /// Invoked after each recorded call so a test can fulfill an expectation once
    /// the asynchronously-delivered drive has been applied.
    var onCall: (() -> Void)?

    func setValue(surface: SurfaceId, element: ElementId, value: JSONValue) -> Bool {
        calls.append(.setValue(surface, element, value)); onCall?(); return true
    }

    func click(surface: SurfaceId, element: ElementId, point: SurfacePoint?) -> Bool {
        calls.append(.click(surface, element, point)); onCall?(); return true
    }

    func navigate(surface: SurfaceId, route: Route) -> Bool {
        calls.append(.navigate(surface, route)); onCall?(); return true
    }

    func render(surface: SurfaceId, component: ComponentSpec) -> Bool {
        calls.append(.render(surface, component)); onCall?(); return true
    }
}

// MARK: - SurfaceDriveControllerTests

/// [Verifies VC-M.1] The agent-`SurfaceOp`→AX-action mapping and the controller's
/// optimistic-concurrency rejection.
///
/// The AX-mapping cases drive ops straight through the production
/// `AccessibilitySurfaceDriveTarget` against a spy root, proving `SetValue` fires
/// `setAccessibilityValue` (with the lowered value) and `Click` fires
/// `accessibilityPerformPress` — and that neither path triggers the other. The
/// rejection case drives a stale `base_version` op through `SurfaceDriveController`
/// and asserts it is surfaced as the drive's `is_error` without reaching the
/// target. See docs/agent/world/ecs-runtime.md (Theme 2a/2c).
final class SurfaceDriveControllerTests: XCTestCase {

    // MARK: AX action mapping (AccessibilitySurfaceDriveTarget)

    func testSetValueMapsToSetAccessibilityValue() {
        let root = SpyAXView()
        let target = AccessibilitySurfaceDriveTarget(root: root)

        let applied = target.setValue(surface: 0, element: 0, value: .string("hello"))

        XCTAssertTrue(applied, "the addressed root element is realized")
        XCTAssertEqual(root.setValueArguments.count, 1, "exactly one setAccessibilityValue fired")
        XCTAssertEqual(root.setValueArguments.first as? String, "hello", "the lowered value reached AX")
        XCTAssertEqual(root.pressCount, 0, "SetValue must not perform a press")
    }

    func testClickMapsToAccessibilityPerformPress() {
        let root = SpyAXView()
        let target = AccessibilitySurfaceDriveTarget(root: root)

        let clicked = target.click(surface: 0, element: 0, point: nil)

        XCTAssertTrue(clicked, "the AX press landed on the realized element")
        XCTAssertEqual(root.pressCount, 1, "exactly one accessibilityPerformPress fired")
        XCTAssertTrue(root.setValueArguments.isEmpty, "Click must not set an AX value")
    }

    func testAbsentElementPerformsNoAXActionAndReportsUnrealized() {
        let root = SpyAXView()
        let target = AccessibilitySurfaceDriveTarget(root: root)

        // Index 1 is out of range (only the root, index 0, exists) → no element.
        let appliedSet = target.setValue(surface: 0, element: 1, value: .string("x"))
        let appliedClick = target.click(surface: 0, element: 1, point: nil)

        XCTAssertFalse(appliedSet, "an unresolved element reports unrealized")
        XCTAssertFalse(appliedClick, "an unresolved element reports unrealized")
        XCTAssertTrue(root.setValueArguments.isEmpty, "no AX value set for an absent element")
        XCTAssertEqual(root.pressCount, 0, "no AX press for an absent element")
    }

    // MARK: Controller dispatch + optimistic-concurrency rejection

    func testControllerDispatchesAcceptedSetValueToTarget() {
        let socket = SurfaceSocket(host: "127.0.0.1", port: 9)
        let target = RecordingDriveTarget()
        let applied = expectation(description: "op applied to target")
        target.onCall = { applied.fulfill() }

        let controller = SurfaceDriveController(socket: socket, target: target) { _, _, _ in
            XCTFail("an accepted op must not report a tool error")
        }

        let drive = SurfaceDrive(
            ops: [.setValue(surface: 1, element: 2, value: .string("hi"), baseVersion: nil)],
            cmd: 5,
            key: IdempotencyKey("tick-5-effect-0")
        )
        socket.driveSubject.send(drive)

        withExtendedLifetime(controller) {
            wait(for: [applied], timeout: 5)
            XCTAssertEqual(
                target.calls,
                [.setValue(1, 2, .string("hi"))],
                "the accepted op dispatched to the setValue seam"
            )
        }
    }

    func testControllerRejectsStaleBaseVersionAsToolError() {
        let socket = SurfaceSocket(host: "127.0.0.1", port: 9)
        let target = RecordingDriveTarget()
        let reported = expectation(description: "stale op reported as tool error")
        var captured: (cmd: CmdId, key: IdempotencyKey, rejections: [SurfaceDriveRejection])?

        let controller = SurfaceDriveController(socket: socket, target: target) { cmd, key, rejections in
            captured = (cmd, key, rejections)
            reported.fulfill()
        }

        // The surface is unobserved (current version 0); the op's precondition
        // (base_version 5) is stale, so it must be rejected without mutating.
        let drive = SurfaceDrive(
            ops: [.setValue(surface: 7, element: 0, value: .string("x"), baseVersion: 5)],
            cmd: 9,
            key: IdempotencyKey("tick-9-effect-0")
        )
        socket.driveSubject.send(drive)

        withExtendedLifetime(controller) {
            wait(for: [reported], timeout: 5)
            XCTAssertEqual(captured?.cmd, 9)
            XCTAssertEqual(captured?.key, IdempotencyKey("tick-9-effect-0"))
            XCTAssertEqual(
                captured?.rejections,
                [SurfaceDriveRejection(surface: 7, baseVersion: 5, current: 0)],
                "the stale precondition is surfaced verbatim"
            )
            XCTAssertTrue(target.calls.isEmpty, "a rejected op never reaches the target")
        }
    }
}
