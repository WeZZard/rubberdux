import CoreGraphics
import XCTest
@testable import Rubberdux

final class BoardGeometryTests: XCTestCase {

    // MARK: - Round trip

    func testCellToPointToCellRoundTrip() {
        let geometry = BoardGeometry(pitch: 20, origin: CGPoint(x: 100, y: 60))
        let cells = [
            GridCell(row: 0, column: 0),
            GridCell(row: 3, column: 7),
            GridCell(row: -2, column: -5),
            GridCell(row: 10, column: -3)
        ]
        for cell in cells {
            let point = geometry.point(for: cell)
            XCTAssertEqual(geometry.cell(at: point), cell, "round trip failed for \(cell)")
        }
    }

    func testCellCenterPoint() {
        let geometry = BoardGeometry(pitch: 20, origin: CGPoint(x: 10, y: 5))
        XCTAssertEqual(geometry.point(for: GridCell(row: 0, column: 0)), CGPoint(x: 10, y: 5))
        XCTAssertEqual(geometry.point(for: GridCell(row: 2, column: 3)), CGPoint(x: 70, y: 45))
    }

    // MARK: - Deterministic point → cell at boundaries

    func testPointToCellDeterministicAtHalfBoundary() {
        let geometry = BoardGeometry(pitch: 10, origin: .zero)
        // Exactly halfway between column 0 (x=0) and column 1 (x=10) is x=5.
        // The boundary rule rounds up, so it resolves to column 1.
        XCTAssertEqual(geometry.cell(at: CGPoint(x: 5, y: 0)).column, 1)
        // Halfway between column -1 (x=-10) and column 0 (x=0) is x=-5;
        // rounds up to column 0.
        XCTAssertEqual(geometry.cell(at: CGPoint(x: -5, y: 0)).column, 0)
        // Just below the boundary stays on the lower cell.
        XCTAssertEqual(geometry.cell(at: CGPoint(x: 4.999, y: 0)).column, 0)
    }

    func testPointToCellSnapsToNearest() {
        let geometry = BoardGeometry(pitch: 10, origin: .zero)
        XCTAssertEqual(geometry.cell(at: CGPoint(x: 12, y: 31)), GridCell(row: 3, column: 1))
        XCTAssertEqual(geometry.cell(at: CGPoint(x: 28, y: 8)), GridCell(row: 1, column: 3))
    }

    // MARK: - Visible-cell enumeration

    func testVisibleCellsEnumeratesViewport() {
        let geometry = BoardGeometry(pitch: 10, origin: .zero)
        // Rect [0,30] x [0,20] inclusive of edge centers → columns 0..3, rows 0..2.
        let rect = CGRect(x: 0, y: 0, width: 30, height: 20)
        let cells = geometry.visibleCells(in: rect)
        XCTAssertEqual(cells.count, 4 * 3)
        XCTAssertEqual(cells.first, GridCell(row: 0, column: 0))
        XCTAssertEqual(cells.last, GridCell(row: 2, column: 3))
    }

    func testVisibleCellsRowMajorOrder() {
        let geometry = BoardGeometry(pitch: 10, origin: .zero)
        let rect = CGRect(x: 0, y: 0, width: 10, height: 10)
        let cells = geometry.visibleCells(in: rect)
        XCTAssertEqual(cells, [
            GridCell(row: 0, column: 0),
            GridCell(row: 0, column: 1),
            GridCell(row: 1, column: 0),
            GridCell(row: 1, column: 1)
        ])
    }

    func testVisibleCellsWithOriginOffset() {
        let geometry = BoardGeometry(pitch: 10, origin: CGPoint(x: 5, y: 5))
        // Centers fall at 5, 15, 25 ... A rect [0,20] x [0,20] catches cells
        // whose centers are at 5 and 15 → columns 0..1, rows 0..1.
        let rect = CGRect(x: 0, y: 0, width: 20, height: 20)
        let cells = geometry.visibleCells(in: rect)
        XCTAssertEqual(cells.count, 4)
        XCTAssertEqual(cells.first, GridCell(row: 0, column: 0))
        XCTAssertEqual(cells.last, GridCell(row: 1, column: 1))
    }

    func testVisibleCellsEmptyForDegenerateRect() {
        let geometry = BoardGeometry(pitch: 10, origin: .zero)
        // A rect entirely between two columns (no center inside) yields nothing.
        let rect = CGRect(x: 1, y: 1, width: 3, height: 3)
        XCTAssertTrue(geometry.visibleCells(in: rect).isEmpty)
    }

    // MARK: - Pitch guard

    func testNonPositivePitchClampedToOne() {
        let geometry = BoardGeometry(pitch: 0, origin: .zero)
        XCTAssertEqual(geometry.pitch, 1)
        // Conversions must not crash and stay consistent.
        XCTAssertEqual(geometry.cell(at: geometry.point(for: GridCell(row: 4, column: 2))),
                       GridCell(row: 4, column: 2))
    }
}
