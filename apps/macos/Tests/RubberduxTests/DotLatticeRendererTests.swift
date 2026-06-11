import CoreGraphics
import XCTest
@testable import Rubberdux

/// Unit tests for the Dock-style magnification falloff. The curve is the sole
/// shared piece of math between the renderer and the view, so it is pinned here
/// against the cosine-bell formula in the approved design.
final class DotLatticeRendererTests: XCTestCase {

    private let accuracy: CGFloat = 1e-9

    // MARK: - Falloff curve

    func testFalloffIsOneAtCursor() {
        // distance 0 → t = 1 → f = 0.5 - 0.5*cos(pi) = 1.
        XCTAssertEqual(DotLatticeRenderer.falloff(distance: 0, radius: 3), 1, accuracy: accuracy)
    }

    func testFalloffIsZeroAtRadius() {
        // distance == radius → t = 0 → f = 0.5 - 0.5*cos(0) = 0.
        XCTAssertEqual(DotLatticeRenderer.falloff(distance: 3, radius: 3), 0, accuracy: accuracy)
    }

    func testFalloffIsZeroBeyondRadius() {
        XCTAssertEqual(DotLatticeRenderer.falloff(distance: 5, radius: 3), 0, accuracy: accuracy)
        XCTAssertEqual(DotLatticeRenderer.falloff(distance: 100, radius: 3), 0, accuracy: accuracy)
    }

    func testFalloffHalfwayMatchesCosineBell() {
        // distance = radius/2 → t = 0.5 → f = 0.5 - 0.5*cos(pi/2) = 0.5.
        XCTAssertEqual(DotLatticeRenderer.falloff(distance: 1.5, radius: 3), 0.5, accuracy: accuracy)
    }

    func testFalloffMatchesFormulaAcrossRange() {
        let radius: CGFloat = 4
        for step in 0...40 {
            let distance = CGFloat(step) / 10
            let t = max(0, min(1, 1 - distance / radius))
            let expected = 0.5 - 0.5 * cos(.pi * t)
            XCTAssertEqual(
                DotLatticeRenderer.falloff(distance: distance, radius: radius),
                expected,
                accuracy: accuracy
            )
        }
    }

    func testFalloffZeroRadiusIsSafe() {
        XCTAssertEqual(DotLatticeRenderer.falloff(distance: 0, radius: 0), 0, accuracy: accuracy)
    }

    // MARK: - Scale

    func testScaleIsMaxAtCursor() {
        XCTAssertEqual(
            DotLatticeRenderer.scale(distance: 0, radius: 3, maxScale: 2.4),
            2.4,
            accuracy: accuracy
        )
    }

    func testScaleIsOneAtAndBeyondRadius() {
        XCTAssertEqual(
            DotLatticeRenderer.scale(distance: 3, radius: 3, maxScale: 2.4),
            1,
            accuracy: accuracy
        )
        XCTAssertEqual(
            DotLatticeRenderer.scale(distance: 10, radius: 3, maxScale: 2.4),
            1,
            accuracy: accuracy
        )
    }

    // MARK: - Opacity

    func testOpacityIsMaxAtCursor() {
        XCTAssertEqual(
            DotLatticeRenderer.opacity(distance: 0, radius: 3, restOpacity: 0.35, maxOpacity: 1),
            1,
            accuracy: 1e-6
        )
    }

    func testOpacityIsRestAtAndBeyondRadius() {
        XCTAssertEqual(
            DotLatticeRenderer.opacity(distance: 3, radius: 3, restOpacity: 0.35, maxOpacity: 1),
            0.35,
            accuracy: 1e-6
        )
        XCTAssertEqual(
            DotLatticeRenderer.opacity(distance: 9, radius: 3, restOpacity: 0.35, maxOpacity: 1),
            0.35,
            accuracy: 1e-6
        )
    }

    // MARK: - Usable-area gate

    func testPointInsideUsableAreaIsAllowed() {
        // A point inside the usable rect may receive a magnified dot / the plus.
        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        XCTAssertTrue(
            DotLatticeRenderer.isWithinUsableArea(CGPoint(x: 80, y: 30), usableBounds: usable)
        )
    }

    func testPointInReservedStripIsRejected() {
        // A point in the reserved strip [140, 200) must be gated out so neither a
        // magnified dot nor the plus glyph is drawn under the floating panel.
        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        XCTAssertFalse(
            DotLatticeRenderer.isWithinUsableArea(CGPoint(x: 160, y: 30), usableBounds: usable)
        )
    }

    func testPointOnTrailingEdgeIsRejected() {
        // The usable rect is half-open at its trailing edge under `contains`, so a
        // point exactly at the inset boundary is treated as reserved.
        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        XCTAssertFalse(
            DotLatticeRenderer.isWithinUsableArea(CGPoint(x: 140, y: 30), usableBounds: usable)
        )
    }

    func testInfiniteUsableAreaAllowsEverything() {
        // Before layout the usable area is `.infinite`, so an un-laid-out renderer
        // magnifies everywhere rather than suppressing all dots.
        XCTAssertTrue(
            DotLatticeRenderer.isWithinUsableArea(CGPoint(x: 9_999, y: 9_999), usableBounds: .infinite)
        )
    }

    // MARK: - Container clip

    func testContainerClipsToUsableAreaAfterLayout() {
        // The renderer clips its container to the laid-out usable rect so every
        // sublayer (lattice, magnified pool, plus glyph, icon tiles) is confined
        // to it; re-laying-out to a wider rect reflows the clip.
        let renderer = DotLatticeRenderer(geometry: BoardGeometry(pitch: 10, origin: .zero))
        XCTAssertTrue(renderer.containerLayer.masksToBounds)

        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        renderer.layout(in: usable)
        XCTAssertEqual(renderer.containerLayer.frame, usable)

        let full = CGRect(x: 0, y: 0, width: 200, height: 100)
        renderer.layout(in: full)
        XCTAssertEqual(renderer.containerLayer.frame, full)
    }
}
