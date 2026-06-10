import CoreGraphics
import Foundation

// MARK: - GridCell

/// An integer lattice coordinate on the board grid. Row increases downward in
/// the board's own coordinate convention; `column` increases rightward. It maps
/// onto an `App`'s `BoardPosition`, but is a pure geometric type that carries no
/// model dependency.
struct GridCell: Equatable, Hashable {
    let row: Int
    let column: Int
}

// MARK: - BoardGeometry

/// Pure cell↔point math for the dot-grid board. It owns the lattice pitch and
/// origin and converts between integer `GridCell` coordinates and view-space
/// `CGPoint`s. It holds no mutable state and performs no drawing, so the renderer
/// and the view share a single deterministic source of geometry.
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
struct BoardGeometry: Equatable {
    /// Center-to-center distance between adjacent lattice dots, in points.
    let pitch: CGFloat

    /// The view-space point that the cell `(row: 0, column: 0)` is centered on.
    let origin: CGPoint

    /// Create a geometry with the given lattice `pitch` and `origin`.
    /// `pitch` must be positive; a non-positive pitch is clamped to `1` so the
    /// conversions never divide by zero.
    init(pitch: CGFloat, origin: CGPoint = .zero) {
        self.pitch = pitch > 0 ? pitch : 1
        self.origin = origin
    }

    // MARK: - Cell → Point

    /// The view-space center point of `cell`.
    func point(for cell: GridCell) -> CGPoint {
        CGPoint(
            x: origin.x + CGFloat(cell.column) * pitch,
            y: origin.y + CGFloat(cell.row) * pitch
        )
    }

    // MARK: - Point → Cell

    /// The lattice cell whose center is nearest to `point`.
    ///
    /// Rounding is deterministic at half-pitch boundaries: a point exactly
    /// halfway between two cells rounds to the higher index, because
    /// `(value).rounded()` uses `.toNearestOrAwayFromZero` and the offset is
    /// shifted to be non-negative before rounding. This guarantees a stable
    /// assignment for points that land precisely on a boundary.
    func cell(at point: CGPoint) -> GridCell {
        GridCell(
            row: nearestIndex((point.y - origin.y) / pitch),
            column: nearestIndex((point.x - origin.x) / pitch)
        )
    }

    /// Round a fractional lattice coordinate to its nearest integer index with a
    /// deterministic rule at the `.5` boundary: half-values always round up
    /// (toward `+∞`), independent of sign. This avoids the away-from-zero
    /// asymmetry of `CGFloat.rounded()` for negative coordinates.
    private func nearestIndex(_ value: CGFloat) -> Int {
        Int((value + 0.5).rounded(.down))
    }

    // MARK: - Visible cells

    /// Every cell whose center lies within `rect`, enumerated row-major
    /// (ascending row, then ascending column). The enumeration is inclusive of
    /// cells centered exactly on the rect's edges.
    ///
    /// Callers pass a viewport (optionally inset by a margin) to bound per-frame
    /// work to the on-screen lattice rather than the full board.
    func visibleCells(in rect: CGRect) -> [GridCell] {
        guard rect.width >= 0, rect.height >= 0 else { return [] }

        let minColumn = ceilIndex((rect.minX - origin.x) / pitch)
        let maxColumn = floorIndex((rect.maxX - origin.x) / pitch)
        let minRow = ceilIndex((rect.minY - origin.y) / pitch)
        let maxRow = floorIndex((rect.maxY - origin.y) / pitch)

        guard minColumn <= maxColumn, minRow <= maxRow else { return [] }

        var cells: [GridCell] = []
        cells.reserveCapacity((maxRow - minRow + 1) * (maxColumn - minColumn + 1))
        for row in minRow...maxRow {
            for column in minColumn...maxColumn {
                cells.append(GridCell(row: row, column: column))
            }
        }
        return cells
    }

    /// The smallest integer index `>=` `value`, i.e. the first cell at or after a
    /// lattice coordinate.
    private func ceilIndex(_ value: CGFloat) -> Int {
        Int(value.rounded(.up))
    }

    /// The largest integer index `<=` `value`, i.e. the last cell at or before a
    /// lattice coordinate.
    private func floorIndex(_ value: CGFloat) -> Int {
        Int(value.rounded(.down))
    }
}
