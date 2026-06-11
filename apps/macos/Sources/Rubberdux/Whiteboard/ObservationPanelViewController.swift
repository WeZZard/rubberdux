import AppKit

// MARK: - ObservationSelectionPlan

/// The pure mapping from a board selection to the set of observation columns the
/// panel should display. It is order-stable so the panel's column order does not
/// jump as the selection set mutates, and it makes the collapse decision (an
/// empty selection hides the panel) testable without any AppKit objects.
///
/// `ObservationPanelViewController` consumes this to reconcile its child
/// `AppObservationViewController`s against the latest selection.
struct ObservationSelectionPlan: Equatable {
    /// The ordered app ids to show, deduplicated, preserving first-seen order.
    let appIDs: [String]

    /// Whether the panel should be visible. The panel collapses when nothing is
    /// selected.
    var isVisible: Bool { !appIDs.isEmpty }

    /// Build a plan from a selection set, ordered by a stable reference order
    /// (the board's app ordering) so columns do not reshuffle on every change.
    /// Ids in `selection` that are absent from `order` are appended afterward in
    /// sorted order, so a selection is never silently dropped.
    init(selection: Set<String>, order: [String]) {
        self.appIDs = ObservationSelectionPlan.orderedColumnIDs(selection: selection, order: order)
    }

    /// The ordered, deduplicated column ids for a selection, ranked by the
    /// board's app ordering. Ids present in `selection` but absent from `order`
    /// are appended afterward in sorted order so a selection is never dropped.
    ///
    /// This is the pure column-layout decision: the panel adds, removes, and
    /// reorders its per-App columns to match this list, one column per id.
    static func orderedColumnIDs(selection: Set<String>, order: [String]) -> [String] {
        var seen = Set<String>()
        var result: [String] = []
        for id in order where selection.contains(id) && !seen.contains(id) {
            seen.insert(id)
            result.append(id)
        }
        for id in selection.subtracting(seen).sorted() {
            result.append(id)
        }
        return result
    }
}

// MARK: - ObservationColumn

/// A single App's column in the observation panel: its observation body
/// (conversation + trajectory + debug box) stacked over its inline pending
/// interactions list. The column owns both child controllers so the panel can
/// reconcile columns as whole units while routing per-App interaction updates to
/// the matching column's pending list.
final class ObservationColumn: NSViewController {

    let appID: String
    let observation: AppObservationViewController
    let pending = PendingInteractionsController()

    init(appID: String, apiClient: APIClient, registry: AppSocketRegistry) {
        self.appID = appID
        self.observation = AppObservationViewController(
            appID: appID,
            apiClient: apiClient,
            registry: registry
        )
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        addChild(observation)
        addChild(pending)

        // Stack the observation body over the inline pending list. The pending
        // list hugs the bottom at its intrinsic height; the observation body
        // takes the remaining space.
        let stack = NSStackView(views: [observation.view, pending.view])
        stack.orientation = .vertical
        stack.spacing = 8
        stack.distribution = .fill
        stack.translatesAutoresizingMaskIntoConstraints = false
        observation.view.setContentHuggingPriority(.defaultLow, for: .vertical)
        pending.view.setContentHuggingPriority(.defaultHigh, for: .vertical)

        let container = NSView()
        container.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: container.topAnchor),
            stack.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor),
        ])
        view = container
    }

    /// Release the column's per-App socket. The owner calls this before removing
    /// the column so the registry ref-count drops and the underlying connection
    /// closes when no other column references the App.
    func teardown() {
        observation.teardown()
    }
}

// MARK: - ObservationPanelViewController

/// The trailing observation panel of the whiteboard. For each selected app it
/// hosts an `ObservationColumn` (the App's observation body plus its inline
/// pending interactions) inside a horizontal, resizable `NSSplitView`, so
/// multi-select lays the apps out side-by-side as draggable columns rather than
/// stacking them as tabs. The panel collapses (its hosting split item is hidden
/// by `WhiteboardViewController`) when the selection is empty.
///
/// Per-app live streams are opened/closed through the shared `AppSocketRegistry`
/// so reconciling the selection never leaks WebSocket connections.
final class ObservationPanelViewController: NSViewController {

    // MARK: - Collaborators

    private let apiClient: APIClient
    private let registry: AppSocketRegistry

    /// Forwarded the human's answer to one of an App's inline pending
    /// interactions, tagged with the app id of the column it came from. The owner
    /// delivers it to the backend and refreshes the panel's interactions for that
    /// App.
    var onRespond: ((String, InteractionResponse) -> Void)?

    /// Reports the panel's current width whenever the user drags the leading
    /// resize handle. The owner (`WhiteboardViewController`) mirrors this into the
    /// board's right inset so the board reflows to the panel's left edge.
    var onWidthChange: ((CGFloat) -> Void)?

    // MARK: - Width

    /// The narrowest the panel may be dragged to, in points.
    static let minimumWidth: CGFloat = 260

    /// The widest the panel may be dragged to, in points.
    static let maximumWidth: CGFloat = 720

    /// The panel's current width. Owned by the panel so the board's reserved
    /// inset is always derived from this single source. Adjusted by the leading
    /// resize handle and clamped to `[minimumWidth, maximumWidth]`.
    private(set) var panelWidth: CGFloat = 360

    /// The Auto Layout constraint that fixes the panel's width; the resize handle
    /// mutates its `constant`.
    private var widthConstraint: NSLayoutConstraint?

    // MARK: - State

    /// The currently displayed plan, used to diff against incoming selections.
    private(set) var plan = ObservationSelectionPlan(selection: [], order: [])

    /// The per-App column controller, keyed by app id.
    private var columns: [String: ObservationColumn] = [:]

    private let splitView = NSSplitView()
    private let emptyLabel = NSTextField(labelWithString: "Select an app to observe it.")

    /// The blurred backing that gives the panel its floating, translucent look.
    private let materialView = NSVisualEffectView()

    /// The draggable strip on the panel's leading edge; dragging it resizes the
    /// panel width.
    private lazy var resizeHandle = PanelResizeHandle(onDrag: { [weak self] deltaX in
        self?.applyResizeDelta(deltaX)
    })

    /// Width of the leading resize handle strip.
    private static let handleWidth: CGFloat = 8

    // MARK: - Init

    init(apiClient: APIClient, registry: AppSocketRegistry) {
        self.apiClient = apiClient
        self.registry = registry
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: - Lifecycle

    override func loadView() {
        // The panel is a floating overlay pinned to the board's right edge: a
        // translucent material card with rounded corners and a drop shadow, fronted
        // by a leading-edge resize handle. The container carries the shadow (which
        // must extend beyond the card), while the material view is clipped to the
        // rounded corners.
        let container = NSView()
        container.wantsLayer = true
        container.shadow = floatingShadow()

        materialView.translatesAutoresizingMaskIntoConstraints = false
        materialView.material = .sidebar
        materialView.blendingMode = .behindWindow
        materialView.state = .active
        materialView.wantsLayer = true
        materialView.layer?.cornerRadius = 12
        materialView.layer?.masksToBounds = true
        container.addSubview(materialView)

        splitView.translatesAutoresizingMaskIntoConstraints = false
        splitView.isVertical = true
        splitView.dividerStyle = .thin
        materialView.addSubview(splitView)

        emptyLabel.translatesAutoresizingMaskIntoConstraints = false
        emptyLabel.textColor = .secondaryLabelColor
        emptyLabel.alignment = .center
        materialView.addSubview(emptyLabel)

        resizeHandle.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(resizeHandle)

        let width = container.widthAnchor.constraint(equalToConstant: panelWidth)
        width.priority = .defaultHigh
        widthConstraint = width

        NSLayoutConstraint.activate([
            width,

            materialView.topAnchor.constraint(equalTo: container.topAnchor),
            materialView.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            materialView.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            materialView.trailingAnchor.constraint(equalTo: container.trailingAnchor),

            // The handle overlays the leading edge so a drag anywhere along the
            // panel's left border resizes it.
            resizeHandle.topAnchor.constraint(equalTo: container.topAnchor),
            resizeHandle.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            resizeHandle.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            resizeHandle.widthAnchor.constraint(equalToConstant: Self.handleWidth),

            splitView.topAnchor.constraint(equalTo: materialView.topAnchor, constant: 8),
            splitView.bottomAnchor.constraint(equalTo: materialView.bottomAnchor, constant: -8),
            splitView.leadingAnchor.constraint(equalTo: materialView.leadingAnchor, constant: Self.handleWidth),
            splitView.trailingAnchor.constraint(equalTo: materialView.trailingAnchor, constant: -8),

            emptyLabel.centerXAnchor.constraint(equalTo: materialView.centerXAnchor),
            emptyLabel.centerYAnchor.constraint(equalTo: materialView.centerYAnchor),
        ])

        view = container
        reflectEmptyState()
    }

    /// The soft drop shadow that lifts the floating card off the board.
    private func floatingShadow() -> NSShadow {
        let shadow = NSShadow()
        shadow.shadowColor = NSColor.black.withAlphaComponent(0.35)
        shadow.shadowBlurRadius = 18
        shadow.shadowOffset = NSSize(width: -4, height: 0)
        return shadow
    }

    // MARK: - Resize

    /// Apply a horizontal drag delta from the leading handle to the panel width.
    /// Because the handle is on the leading (left) edge while the panel is pinned
    /// to the right, dragging left (`deltaX < 0`) widens the panel.
    private func applyResizeDelta(_ deltaX: CGFloat) {
        setWidth(panelWidth - deltaX)
    }

    /// Set the panel width, clamped to `[minimumWidth, maximumWidth]`, update the
    /// width constraint, and report the new width so the board reflows.
    private func setWidth(_ proposed: CGFloat) {
        let clamped = min(max(proposed, Self.minimumWidth), Self.maximumWidth)
        guard clamped != panelWidth else { return }
        panelWidth = clamped
        widthConstraint?.constant = clamped
        onWidthChange?(clamped)
    }

    // MARK: - Selection

    /// Reconcile the displayed columns against a new board selection. `order` is
    /// the board's app ordering, used to keep column order stable. Returns the
    /// resulting plan so the owner (`WhiteboardViewController`) can
    /// collapse/expand the hosting split item from `plan.isVisible`.
    @discardableResult
    func update(selection: Set<String>, order: [String]) -> ObservationSelectionPlan {
        let next = ObservationSelectionPlan(selection: selection, order: order)
        guard next != plan else { return next }

        let nextIDs = Set(next.appIDs)
        let currentIDs = Set(plan.appIDs)

        // Remove columns that are no longer selected, releasing their sockets.
        for id in currentIDs.subtracting(nextIDs) {
            if let column = columns.removeValue(forKey: id) {
                column.view.removeFromSuperview()
                column.teardown()
                column.removeFromParent()
            }
        }

        // Add columns for newly selected apps.
        for id in next.appIDs where columns[id] == nil {
            let column = ObservationColumn(appID: id, apiClient: apiClient, registry: registry)
            column.pending.onRespond = { [weak self] response in
                self?.onRespond?(id, response)
            }
            columns[id] = column
            addChild(column)
            splitView.addArrangedSubview(column.view)
        }

        // Reorder columns to match the plan order.
        reorderColumns(to: next.appIDs)

        plan = next
        reflectEmptyState()
        return next
    }

    // MARK: - Interactions

    /// Push the latest pending interactions for `appID` into the matching
    /// column's inline list. No-op when no column shows the App, so an update for
    /// an unselected App is harmlessly ignored.
    func updateInteractions(_ interactions: [AgentInteraction], forAppID appID: String) {
        columns[appID]?.pending.setInteractions(interactions)
    }

    // MARK: - Private

    private func reorderColumns(to ids: [String]) {
        for (index, id) in ids.enumerated() {
            guard let column = columns[id] else { continue }
            let view = column.view
            let currentIndex = splitView.arrangedSubviews.firstIndex(of: view)
            if currentIndex != index {
                splitView.removeArrangedSubview(view)
                splitView.insertArrangedSubview(view, at: index)
            }
        }
    }

    private func reflectEmptyState() {
        let empty = plan.appIDs.isEmpty
        splitView.isHidden = empty
        emptyLabel.isHidden = !empty
    }
}

// MARK: - PanelResizeHandle

/// The draggable strip on the floating panel's leading edge. It reports each
/// horizontal mouse-drag delta to its owner, which translates it into a panel
/// width change. It also shows the horizontal-resize cursor while the pointer is
/// over it so the affordance is discoverable.
final class PanelResizeHandle: NSView {

    /// Called with the horizontal delta (in the handle's coordinate space) on
    /// every drag step, so the owner can resize the panel incrementally.
    private let onDrag: (CGFloat) -> Void

    init(onDrag: @escaping (CGFloat) -> Void) {
        self.onDrag = onDrag
        super.init(frame: .zero)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("PanelResizeHandle is created programmatically, not from a coder")
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .resizeLeftRight)
    }

    override func mouseDragged(with event: NSEvent) {
        // `deltaX` is the incremental horizontal movement since the last event,
        // which the owner accumulates into the panel width.
        onDrag(event.deltaX)
    }
}
