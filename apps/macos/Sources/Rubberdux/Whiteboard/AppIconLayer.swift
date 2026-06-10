import AppKit
import QuartzCore

// MARK: - AppIconLayer

/// A `CALayer` rendering one app's icon on the board: a rounded color tile with
/// an SF-Symbol glyph, snapped to its cell center via `BoardGeometry`. A
/// selection-ring sublayer and a count badge sublayer sit above the tile.
///
/// The tile color comes from `AppPalette` and the glyph from `SymbolImage`, so
/// the icon's appearance is derived from the same helpers the rest of the
/// whiteboard client uses — never re-implemented here.
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
final class AppIconLayer: CALayer {

    // MARK: - Sublayers

    /// The SF-Symbol glyph drawn centered on the tile.
    private let glyphLayer = CALayer()

    /// The selection ring drawn around the tile; hidden unless selected.
    private let selectionRingLayer = CALayer()

    /// The unread-count badge in the top-trailing corner; hidden at zero.
    private let badgeLayer = CATextLayer()

    /// The cell this icon is snapped to. The view repositions the layer when the
    /// app moves; geometry conversion happens in `place(in:)`.
    private(set) var cell: GridCell

    // MARK: - Appearance state

    /// Whether the icon shows its selection ring.
    var isSelected: Bool = false {
        didSet { applySelection() }
    }

    /// Whether the cursor is hovering this icon; lifts the tile slightly.
    var isHovered: Bool = false {
        didSet { applyHover() }
    }

    /// The badge count. `0` hides the badge.
    var badgeCount: Int = 0 {
        didSet { applyBadge() }
    }

    private let tileSide: CGFloat

    // MARK: - Init

    /// Build an icon layer for `app` with a tile of side `tileSide` points.
    init(app: App, tileSide: CGFloat = 44) {
        self.cell = GridCell(row: app.position.row, column: app.position.column)
        self.tileSide = tileSide
        super.init()
        configureTile(for: app)
        configureGlyph(for: app)
        configureSelectionRing()
        configureBadge()
        applySelection()
        applyBadge()
    }

    /// Required by `CALayer`. Used by Core Animation when copying the layer (e.g.
    /// for presentation); the copied layer shares this layer's geometry.
    override init(layer: Any) {
        if let other = layer as? AppIconLayer {
            self.cell = other.cell
            self.tileSide = other.tileSide
        } else {
            self.cell = GridCell(row: 0, column: 0)
            self.tileSide = 44
        }
        super.init(layer: layer)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("AppIconLayer is created programmatically, not from a coder")
    }

    // MARK: - Placement

    /// Snap this icon to its cell's center in `geometry` and lay out sublayers.
    func place(in geometry: BoardGeometry) {
        let center = geometry.point(for: cell)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        bounds = CGRect(x: 0, y: 0, width: tileSide, height: tileSide)
        position = center
        layoutDecorations()
        CATransaction.commit()
    }

    /// Update the cell this icon is bound to (e.g. after the app is moved). The
    /// caller follows with `place(in:)` to apply the new geometry.
    func move(to cell: GridCell) {
        self.cell = cell
    }

    // MARK: - Configuration

    private func configureTile(for app: App) {
        backgroundColor = AppPalette.color(named: app.icon.color).cgColor
        cornerRadius = tileSide * 0.22
        masksToBounds = false
        shadowColor = NSColor.black.cgColor
        shadowOpacity = 0.18
        shadowRadius = 3
        shadowOffset = CGSize(width: 0, height: -1)
    }

    private func configureGlyph(for app: App) {
        let symbol = SymbolImage.image(named: app.icon.symbol)
        let tinted = symbol.tinted(with: .white)
        glyphLayer.contents = tinted.cgImageForLayer()
        glyphLayer.contentsGravity = .resizeAspect
        addSublayer(glyphLayer)
    }

    private func configureSelectionRing() {
        selectionRingLayer.backgroundColor = NSColor.clear.cgColor
        selectionRingLayer.borderColor = NSColor.controlAccentColor.cgColor
        selectionRingLayer.borderWidth = 2.5
        selectionRingLayer.cornerRadius = tileSide * 0.22 + 3
        selectionRingLayer.isHidden = true
        addSublayer(selectionRingLayer)
    }

    private func configureBadge() {
        badgeLayer.alignmentMode = .center
        badgeLayer.foregroundColor = NSColor.white.cgColor
        badgeLayer.backgroundColor = NSColor.systemRed.cgColor
        badgeLayer.fontSize = 11
        badgeLayer.contentsScale = NSScreen.main?.backingScaleFactor ?? 2
        badgeLayer.isHidden = true
        addSublayer(badgeLayer)
    }

    // MARK: - Layout

    private func layoutDecorations() {
        let glyphInset = tileSide * 0.22
        glyphLayer.frame = bounds.insetBy(dx: glyphInset, dy: glyphInset)

        let ringInset: CGFloat = -3
        selectionRingLayer.frame = bounds.insetBy(dx: ringInset, dy: ringInset)

        let badgeSide: CGFloat = 18
        badgeLayer.frame = CGRect(
            x: bounds.maxX - badgeSide * 0.7,
            y: bounds.maxY - badgeSide * 0.7,
            width: badgeSide,
            height: badgeSide
        )
        badgeLayer.cornerRadius = badgeSide / 2
    }

    // MARK: - State application

    private func applySelection() {
        selectionRingLayer.isHidden = !isSelected
    }

    private func applyHover() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        transform = isHovered
            ? CATransform3DMakeScale(1.08, 1.08, 1)
            : CATransform3DIdentity
        shadowOpacity = isHovered ? 0.30 : 0.18
        CATransaction.commit()
    }

    private func applyBadge() {
        if badgeCount > 0 {
            badgeLayer.isHidden = false
            badgeLayer.string = badgeCount > 99 ? "99+" : String(badgeCount)
        } else {
            badgeLayer.isHidden = true
            badgeLayer.string = nil
        }
    }
}

// MARK: - NSImage helpers

private extension NSImage {
    /// A copy of this template/symbol image tinted with `color`, for use as a
    /// glyph over the colored tile.
    func tinted(with color: NSColor) -> NSImage {
        let image = NSImage(size: size)
        image.lockFocus()
        color.set()
        let rect = NSRect(origin: .zero, size: size)
        draw(at: .zero, from: rect, operation: .sourceOver, fraction: 1)
        rect.fill(using: .sourceAtop)
        image.unlockFocus()
        image.isTemplate = false
        return image
    }

    /// A `CGImage` suitable for `CALayer.contents`, evaluated at the image's own
    /// size.
    func cgImageForLayer() -> CGImage? {
        var rect = CGRect(origin: .zero, size: size)
        return cgImage(forProposedRect: &rect, context: nil, hints: nil)
    }
}
