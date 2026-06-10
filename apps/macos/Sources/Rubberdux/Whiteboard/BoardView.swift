import AppKit
import QuartzCore

// MARK: - BoardHit

/// The result of hit-testing a board point: either an app icon was hit, or an
/// empty cell. The view controller (a later task) decides what each means; the
/// board view only classifies.
enum BoardHit: Equatable {
    /// An app icon was hit. Carries the app's identifier.
    case icon(appID: String)
    /// No icon under the point; the nearest empty cell is reported so the caller
    /// can offer a create affordance there.
    case emptyCell(GridCell)
}

// MARK: - BoardViewDelegate

/// Callbacks the board view raises for interaction. The view itself stays free
/// of app/session logic; it forwards classified events.
protocol BoardViewDelegate: AnyObject {
    /// The user clicked an app icon.
    func boardView(_ boardView: BoardView, didSelectAppID appID: String)
    /// The user clicked an empty cell.
    func boardView(_ boardView: BoardView, didActivateEmptyCell cell: GridCell)
}

// MARK: - BoardView

/// The layer-backed dot-grid board. It hosts the `DotLatticeRenderer`'s lattice
/// and the per-app `AppIconLayer`s, tracks the mouse via an `NSTrackingArea`,
/// and drives cursor-follow magnification on `mouseMoved`. Hit-testing routes a
/// click to either an icon or an empty cell.
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
final class BoardView: NSView {

    // MARK: - Collaborators

    weak var delegate: BoardViewDelegate?

    private let geometry: BoardGeometry
    private let renderer: DotLatticeRenderer

    /// Icon layers keyed by app id, so updates (move, badge, selection) are
    /// `O(1)` and the at-rest field is never walked to find an icon.
    private var iconLayers: [String: AppIconLayer] = [:]

    /// The currently selected app id, if any.
    private(set) var selectedAppID: String?

    private var trackingArea: NSTrackingArea?

    // MARK: - Init

    /// Build a board view with the given lattice `geometry`. The view is
    /// layer-backed and flipped so the board's row-down convention matches the
    /// view coordinate space.
    init(geometry: BoardGeometry) {
        self.geometry = geometry
        self.renderer = DotLatticeRenderer(geometry: geometry)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.addSublayer(renderer.containerLayer)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("BoardView is created programmatically, not from a coder")
    }

    // MARK: - Coordinate convention

    /// Use a flipped coordinate system so increasing `row` runs downward, matching
    /// `BoardPosition` and `BoardGeometry`'s row convention.
    override var isFlipped: Bool { true }

    // MARK: - Layout

    override func layout() {
        super.layout()
        renderer.layout(in: bounds)
        for icon in iconLayers.values {
            icon.place(in: geometry)
        }
    }

    // MARK: - Apps

    /// Replace the set of displayed apps. Active apps get an `AppIconLayer`;
    /// tombstoned apps are omitted. Existing layers for apps no longer present
    /// are removed.
    func setApps(_ apps: [App]) {
        let active = apps.filter { $0.status == .active }
        let nextIDs = Set(active.map(\.id))

        for (id, layer) in iconLayers where !nextIDs.contains(id) {
            layer.removeFromSuperlayer()
            iconLayers.removeValue(forKey: id)
        }

        for app in active {
            if let existing = iconLayers[app.id] {
                existing.move(to: GridCell(row: app.position.row, column: app.position.column))
                existing.place(in: geometry)
            } else {
                let layer = AppIconLayer(app: app)
                iconLayers[app.id] = layer
                renderer.containerLayer.addSublayer(layer)
                layer.place(in: geometry)
            }
        }
    }

    /// Set the unread badge count for an app.
    func setBadge(_ count: Int, forAppID appID: String) {
        iconLayers[appID]?.badgeCount = count
    }

    /// The view-space rect of `appID`'s icon tile, clamped to the board's visible
    /// bounds so a popover anchored to it never points off-screen. Returns `nil`
    /// when no icon exists for `appID`. Used to anchor the interaction popover and
    /// the pending-list pop-out to an icon.
    func iconRect(forAppID appID: String) -> NSRect? {
        guard let layer = iconLayers[appID] else { return nil }
        let clamped = layer.frame.intersection(bounds)
        return clamped.isNull ? layer.frame : clamped
    }

    /// Select an app icon (or clear selection with `nil`), updating rings.
    func selectApp(_ appID: String?) {
        if let current = selectedAppID, let layer = iconLayers[current] {
            layer.isSelected = false
        }
        selectedAppID = appID
        if let appID, let layer = iconLayers[appID] {
            layer.isSelected = true
        }
    }

    // MARK: - Hit testing

    /// Classify `point` (view coordinates) as an icon hit or an empty cell.
    /// An icon is hit when the point falls within its tile frame; otherwise the
    /// nearest cell is reported as empty.
    func hit(at point: CGPoint) -> BoardHit {
        for (id, layer) in iconLayers where layer.frame.contains(point) {
            return .icon(appID: id)
        }
        return .emptyCell(geometry.cell(at: point))
    }

    /// Whether `cell` currently has no app icon, used to gate the plus-glyph.
    private func isCellEmpty(_ cell: GridCell) -> Bool {
        !iconLayers.values.contains { $0.cell == cell }
    }

    // MARK: - Tracking

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea {
            removeTrackingArea(trackingArea)
        }
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        trackingArea = area
    }

    override func mouseMoved(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        updateHover(at: point)
        renderer.magnify(at: point, isCellEmpty: isCellEmpty)
    }

    override func mouseExited(with event: NSEvent) {
        renderer.clearMagnification()
        clearHover()
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        switch hit(at: point) {
        case let .icon(appID):
            selectApp(appID)
            delegate?.boardView(self, didSelectAppID: appID)
        case let .emptyCell(cell):
            delegate?.boardView(self, didActivateEmptyCell: cell)
        }
    }

    // MARK: - Hover

    private var hoveredAppID: String?

    private func updateHover(at point: CGPoint) {
        let next: String?
        if case let .icon(appID) = hit(at: point) {
            next = appID
        } else {
            next = nil
        }
        guard next != hoveredAppID else { return }
        if let previous = hoveredAppID {
            iconLayers[previous]?.isHovered = false
        }
        hoveredAppID = next
        if let next {
            iconLayers[next]?.isHovered = true
        }
    }

    private func clearHover() {
        if let previous = hoveredAppID {
            iconLayers[previous]?.isHovered = false
        }
        hoveredAppID = nil
    }
}
