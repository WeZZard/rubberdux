import CoreGraphics
import XCTest
@testable import Rubberdux

/// Covers the usable-area inset geometry introduced for the floating observation
/// panel: a point in the usable area still maps to its nearest cell, a point in
/// the reserved (panel) region maps to no cell, and changing the inset changes
/// the mapping. The reserved region is `bounds` minus a right inset equal to the
/// panel width.
final class BoardGeometryInsetTests: XCTestCase {

    private let geometry = BoardGeometry(pitch: 10, origin: .zero)

    // MARK: - Usable area maps to a cell

    func testPointInsideUsableAreaMapsToCell() {
        // A 200-wide board with a 60-point right inset has a usable area of
        // [0, 140) in x. A point at x = 80 lies inside it and maps to its cell.
        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        let cell = geometry.cell(at: CGPoint(x: 80, y: 30), in: usable)
        XCTAssertEqual(cell, GridCell(row: 3, column: 8))
    }

    func testCellMapsWithinBoundsMinusRightInset() {
        // The cell reported for any point in the usable area must have a center
        // that lies at or before the usable area's trailing edge.
        let bounds = CGRect(x: 0, y: 0, width: 200, height: 100)
        let rightInset: CGFloat = 60
        let usable = CGRect(
            x: bounds.minX,
            y: bounds.minY,
            width: bounds.width - rightInset,
            height: bounds.height
        )
        // A point just inside the usable edge maps to a cell whose center's x is
        // within the usable width.
        let point = CGPoint(x: usable.maxX - 1, y: 50)
        let cell = geometry.cell(at: point, in: usable)
        XCTAssertNotNil(cell)
        if let cell {
            XCTAssertLessThanOrEqual(geometry.point(for: cell).x, usable.maxX)
        }
    }

    // MARK: - Reserved region maps to no cell

    func testPointInReservedRegionMapsToNoCell() {
        // x = 160 lies in the reserved strip [140, 200) and must map to no cell,
        // so the caller offers no create affordance under the panel.
        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        XCTAssertNil(geometry.cell(at: CGPoint(x: 160, y: 30), in: usable))
    }

    func testPointExactlyOnUsableTrailingEdgeIsReserved() {
        // The usable rect is half-open at its trailing edge under `contains`, so a
        // point exactly at the inset boundary counts as reserved.
        let usable = CGRect(x: 0, y: 0, width: 140, height: 100)
        XCTAssertNil(geometry.cell(at: CGPoint(x: 140, y: 30), in: usable))
    }

    // MARK: - Changing the inset changes the mapping

    func testChangingInsetChangesMapping() {
        let point = CGPoint(x: 160, y: 30)
        // With a small inset the point is in the usable area and maps to a cell.
        let wideUsable = CGRect(x: 0, y: 0, width: 180, height: 100)
        XCTAssertEqual(geometry.cell(at: point, in: wideUsable), GridCell(row: 3, column: 16))
        // Growing the inset (shrinking the usable area) pushes the same point into
        // the reserved region, so it now maps to no cell.
        let narrowUsable = CGRect(x: 0, y: 0, width: 140, height: 100)
        XCTAssertNil(geometry.cell(at: point, in: narrowUsable))
    }

    // MARK: - Zero inset preserves the unrestricted mapping

    func testZeroInsetMatchesUnrestrictedCell() {
        // A usable rect equal to the full bounds must agree with the plain
        // `cell(at:)` for any point inside it.
        let bounds = CGRect(x: 0, y: 0, width: 200, height: 100)
        for point in [CGPoint(x: 5, y: 5), CGPoint(x: 95, y: 45), CGPoint(x: 195, y: 95)] {
            XCTAssertEqual(geometry.cell(at: point, in: bounds), geometry.cell(at: point))
        }
    }
}
