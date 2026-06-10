import AppKit
import QuartzCore

// MARK: - DotLatticeRenderer

/// Renders the board's dot field: an at-rest lattice of gray dots that costs
/// ~zero to maintain, plus a fixed reusable pool of "magnified" dot layers that
/// follow the cursor with Dock-style magnification.
///
/// The at-rest field is a single `CAReplicatorLayer`: one prototype dot tiled
/// across rows and columns. No per-dot layer is ever stored, so the resting cost
/// is independent of grid size. On each cursor move only the dots within the
/// influence radius are mutated, drawn by the pool — cost is `O(pool)`, not
/// `O(grid)`.
///
/// A single plus-glyph layer fades in over the magnified center cell, but only
/// when the caller reports that cell is empty (no app icon there).
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
final class DotLatticeRenderer {

    // MARK: - Tuning

    /// Geometry of magnification, kept in one value so the falloff math and the
    /// pool sizing stay in agreement.
    struct Magnification: Equatable {
        /// Influence radius in points: dots farther than this from the cursor are
        /// not magnified. Expressed by the caller as a multiple of the pitch
        /// (≈3 cells).
        let radius: CGFloat

        /// Scale applied to a dot exactly under the cursor (`distance == 0`).
        let maxScale: CGFloat

        /// Opacity applied to a dot exactly under the cursor.
        let maxOpacity: Float

        /// Opacity of an at-rest dot, used as the floor the magnified opacity
        /// rises from.
        let restOpacity: Float

        static let `default` = Magnification(
            radius: 3,
            maxScale: 2.4,
            maxOpacity: 1.0,
            restOpacity: 0.35
        )
    }

    // MARK: - Falloff

    /// The cosine-bell falloff used by Dock-style magnification.
    ///
    /// For a base dot whose center is `distance` points from the cursor and an
    /// influence `radius`, the normalized closeness is
    /// `t = clamp(1 − distance/radius, 0, 1)` and the bell is
    /// `f = 0.5 − 0.5·cos(π·t)`. `f` is `1` at the cursor (`distance == 0`),
    /// falls smoothly to `0` at `distance == radius`, and stays `0` beyond it.
    ///
    /// Defined as a pure static function so the view and the unit tests share a
    /// single source of truth for the curve.
    static func falloff(distance: CGFloat, radius: CGFloat) -> CGFloat {
        guard radius > 0 else { return 0 }
        let t = max(0, min(1, 1 - distance / radius))
        return 0.5 - 0.5 * cos(.pi * t)
    }

    /// The scale a dot receives at `distance` from the cursor: `1` at and beyond
    /// `radius`, `maxScale` at `distance == 0`.
    static func scale(distance: CGFloat, radius: CGFloat, maxScale: CGFloat) -> CGFloat {
        1 + (maxScale - 1) * falloff(distance: distance, radius: radius)
    }

    /// The opacity a dot receives at `distance` from the cursor: `restOpacity` at
    /// and beyond `radius`, `maxOpacity` at `distance == 0`.
    static func opacity(
        distance: CGFloat,
        radius: CGFloat,
        restOpacity: Float,
        maxOpacity: Float
    ) -> Float {
        let f = Float(falloff(distance: distance, radius: radius))
        return restOpacity + (maxOpacity - restOpacity) * f
    }

    // MARK: - Layers

    /// The host layer the renderer attaches its sublayers to.
    let containerLayer = CALayer()

    /// The at-rest tiled field. Replicated rows × columns of one prototype dot.
    private let rowReplicator = CAReplicatorLayer()
    private let columnReplicator = CAReplicatorLayer()
    private let prototypeDot = CALayer()

    /// The reusable pool of magnified-dot layers. Sized once to cover the
    /// influence disc; reused across cursor moves so no allocation happens on the
    /// hot path.
    private var pool: [CALayer] = []

    /// The plus-glyph shown over an empty, hovered center cell.
    private let plusGlyphLayer = CALayer()

    // MARK: - State

    private var geometry: BoardGeometry
    private let magnification: Magnification
    private let dotDiameter: CGFloat
    private let dotColor: CGColor
    private let plusColor: CGColor

    // MARK: - Init

    /// Build a renderer for `geometry`. `dotDiameter` is the at-rest dot size in
    /// points; `magnification.radius` is expressed in pitch multiples.
    init(
        geometry: BoardGeometry,
        magnification: Magnification = .default,
        dotDiameter: CGFloat = 3,
        dotColor: NSColor = .systemGray,
        plusColor: NSColor = .secondaryLabelColor
    ) {
        self.geometry = geometry
        self.magnification = magnification
        self.dotDiameter = dotDiameter
        self.dotColor = dotColor.cgColor
        self.plusColor = plusColor.cgColor

        configurePrototype()
        configureReplicators()
        configurePool()
        configurePlusGlyph()
    }

    // MARK: - Configuration

    private func configurePrototype() {
        prototypeDot.bounds = CGRect(x: 0, y: 0, width: dotDiameter, height: dotDiameter)
        prototypeDot.cornerRadius = dotDiameter / 2
        prototypeDot.backgroundColor = dotColor
        prototypeDot.opacity = magnification.restOpacity
    }

    private func configureReplicators() {
        columnReplicator.addSublayer(prototypeDot)
        rowReplicator.addSublayer(columnReplicator)
        containerLayer.addSublayer(rowReplicator)
        containerLayer.addSublayer(plusGlyphLayer)
        // The pool draws above the at-rest field so magnified dots occlude their
        // replicated counterparts.
        for dot in pool {
            containerLayer.addSublayer(dot)
        }
    }

    private func configurePool() {
        // The pool must cover every cell whose center is within the influence
        // disc: a square of side `2·radius + 1` cells is the tight upper bound.
        let span = Int(magnification.radius.rounded(.up)) * 2 + 1
        let count = span * span
        pool = (0..<count).map { _ in
            let dot = CALayer()
            dot.bounds = CGRect(x: 0, y: 0, width: dotDiameter, height: dotDiameter)
            dot.cornerRadius = dotDiameter / 2
            dot.backgroundColor = dotColor
            dot.opacity = 0
            containerLayer.addSublayer(dot)
            return dot
        }
    }

    private func configurePlusGlyph() {
        let glyph = "+"
        let size: CGFloat = max(dotDiameter * 4, 12)
        plusGlyphLayer.bounds = CGRect(x: 0, y: 0, width: size, height: size)
        plusGlyphLayer.opacity = 0
        plusGlyphLayer.contents = plusImage(side: size).cgImageForLayer()
        plusGlyphLayer.contentsGravity = .resizeAspect
        _ = glyph
    }

    // MARK: - Geometry update

    /// Reposition the at-rest field for a new viewport. The replicator instance
    /// counts cover every visible column and row; only the prototype and the two
    /// replicator transforms change, so this stays `O(1)` in stored layers.
    func layout(in bounds: CGRect) {
        let cells = geometry.visibleCells(in: bounds)
        guard let first = cells.first, let last = cells.last else { return }

        let columns = last.column - first.column + 1
        let rows = last.row - first.row + 1

        CATransaction.begin()
        CATransaction.setDisableActions(true)

        containerLayer.frame = bounds

        let firstPoint = geometry.point(for: first)
        prototypeDot.position = firstPoint

        columnReplicator.instanceCount = max(columns, 1)
        columnReplicator.instanceTransform = CATransform3DMakeTranslation(geometry.pitch, 0, 0)

        rowReplicator.instanceCount = max(rows, 1)
        rowReplicator.instanceTransform = CATransform3DMakeTranslation(0, geometry.pitch, 0)

        CATransaction.commit()
    }

    // MARK: - Magnification

    /// Apply Dock-style magnification centered on `cursor`. `isCellEmpty` reports
    /// whether the cell nearest the cursor has no app icon, which gates the
    /// plus-glyph.
    ///
    /// Only the dots within the influence radius are touched; the work is bounded
    /// by the pool size. The whole update runs inside a `CATransaction` with
    /// implicit actions disabled so the field follows the cursor without
    /// animating each move.
    func magnify(at cursor: CGPoint, isCellEmpty: (GridCell) -> Bool) {
        let radiusPoints = magnification.radius * geometry.pitch
        let influence = CGRect(
            x: cursor.x - radiusPoints,
            y: cursor.y - radiusPoints,
            width: radiusPoints * 2,
            height: radiusPoints * 2
        )
        let cells = geometry.visibleCells(in: influence)

        CATransaction.begin()
        CATransaction.setDisableActions(true)

        var poolIndex = 0
        for cell in cells where poolIndex < pool.count {
            let center = geometry.point(for: cell)
            let distance = hypot(center.x - cursor.x, center.y - cursor.y)
            if distance > radiusPoints { continue }

            let dot = pool[poolIndex]
            poolIndex += 1

            dot.position = center
            dot.transform = CATransform3DMakeScale(
                Self.scale(distance: distance, radius: radiusPoints, maxScale: magnification.maxScale),
                Self.scale(distance: distance, radius: radiusPoints, maxScale: magnification.maxScale),
                1
            )
            dot.opacity = Self.opacity(
                distance: distance,
                radius: radiusPoints,
                restOpacity: magnification.restOpacity,
                maxOpacity: magnification.maxOpacity
            )
        }

        // Park the unused pool dots offscreen-invisible so stale positions never
        // linger after the cursor moves to a sparser region.
        for index in poolIndex..<pool.count {
            pool[index].opacity = 0
        }

        updatePlusGlyph(at: cursor, isCellEmpty: isCellEmpty)

        CATransaction.commit()
    }

    /// Fade the magnified field out — used when the cursor leaves the view.
    func clearMagnification() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for dot in pool {
            dot.opacity = 0
        }
        plusGlyphLayer.opacity = 0
        CATransaction.commit()
    }

    private func updatePlusGlyph(at cursor: CGPoint, isCellEmpty: (GridCell) -> Bool) {
        let cell = geometry.cell(at: cursor)
        if isCellEmpty(cell) {
            plusGlyphLayer.position = geometry.point(for: cell)
            plusGlyphLayer.opacity = 1
        } else {
            plusGlyphLayer.opacity = 0
        }
    }

    // MARK: - Plus glyph image

    private func plusImage(side: CGFloat) -> NSImage {
        let image = NSImage(size: NSSize(width: side, height: side))
        image.lockFocus()
        let thickness: CGFloat = max(side / 8, 1)
        let arm = side * 0.7
        let center = side / 2
        NSColor(cgColor: plusColor)?.setFill()
        // Horizontal arm.
        NSBezierPath(
            rect: NSRect(x: center - arm / 2, y: center - thickness / 2, width: arm, height: thickness)
        ).fill()
        // Vertical arm.
        NSBezierPath(
            rect: NSRect(x: center - thickness / 2, y: center - arm / 2, width: thickness, height: arm)
        ).fill()
        image.unlockFocus()
        return image
    }
}

// MARK: - NSImage → CGImage for layer contents

private extension NSImage {
    /// A `CGImage` suitable for `CALayer.contents`, evaluated at the image's own
    /// size.
    func cgImageForLayer() -> CGImage? {
        var rect = CGRect(origin: .zero, size: size)
        return cgImage(forProposedRect: &rect, context: nil, hints: nil)
    }
}
