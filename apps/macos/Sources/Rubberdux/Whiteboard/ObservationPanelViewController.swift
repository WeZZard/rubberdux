import AppKit

// MARK: - ObservationSelectionPlan

/// The pure mapping from a board selection to the set of observation panes the
/// panel should display. It is order-stable so the panel's tab/stack order does
/// not jump as the selection set mutates, and it makes the collapse decision
/// (an empty selection hides the panel) testable without any AppKit objects.
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
    /// (the board's app ordering) so panes do not reshuffle on every change.
    /// Ids in `selection` that are absent from `order` are appended afterward in
    /// sorted order, so a selection is never silently dropped.
    init(selection: Set<String>, order: [String]) {
        var seen = Set<String>()
        var result: [String] = []
        for id in order where selection.contains(id) && !seen.contains(id) {
            seen.insert(id)
            result.append(id)
        }
        for id in selection.subtracting(seen).sorted() {
            result.append(id)
        }
        self.appIDs = result
    }
}

// MARK: - ObservationPanelViewController

/// The trailing observation panel of the whiteboard. For each selected app it
/// hosts an `AppObservationViewController` (conversation + trajectory + debug
/// message box) in a tabbed container, so multi-select stacks panes as
/// selectable tabs. The panel collapses (its hosting split item is hidden by
/// `WhiteboardViewController`) when the selection is empty.
///
/// Per-app live streams are opened/closed through the shared `AppSocketRegistry`
/// so reconciling the selection never leaks WebSocket connections.
final class ObservationPanelViewController: NSViewController {

    // MARK: - Collaborators

    private let apiClient: APIClient
    private let registry: AppSocketRegistry

    // MARK: - State

    /// The currently displayed plan, used to diff against incoming selections.
    private(set) var plan = ObservationSelectionPlan(selection: [], order: [])

    /// The child controller per displayed app id.
    private var panes: [String: AppObservationViewController] = [:]

    private let tabView = NSTabView()
    private let emptyLabel = NSTextField(labelWithString: "Select an app to observe it.")

    // MARK: - Init

    init(apiClient: APIClient, registry: AppSocketRegistry) {
        self.apiClient = apiClient
        self.registry = registry
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: - Lifecycle

    override func loadView() {
        let container = NSView()

        tabView.translatesAutoresizingMaskIntoConstraints = false
        tabView.tabViewType = .topTabsBezelBorder
        container.addSubview(tabView)

        emptyLabel.translatesAutoresizingMaskIntoConstraints = false
        emptyLabel.textColor = .secondaryLabelColor
        emptyLabel.alignment = .center
        container.addSubview(emptyLabel)

        NSLayoutConstraint.activate([
            tabView.topAnchor.constraint(equalTo: container.topAnchor, constant: 8),
            tabView.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -8),
            tabView.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 8),
            tabView.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -8),

            emptyLabel.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            emptyLabel.centerYAnchor.constraint(equalTo: container.centerYAnchor),
        ])

        view = container
        reflectEmptyState()
    }

    // MARK: - Selection

    /// Reconcile the displayed panes against a new board selection. `order` is the
    /// board's app ordering, used to keep tab order stable. Returns the resulting
    /// plan so the owner (`WhiteboardViewController`) can collapse/expand the
    /// hosting split item from `plan.isVisible`.
    @discardableResult
    func update(selection: Set<String>, order: [String]) -> ObservationSelectionPlan {
        let next = ObservationSelectionPlan(selection: selection, order: order)
        guard next != plan else { return next }

        let nextIDs = Set(next.appIDs)
        let currentIDs = Set(plan.appIDs)

        // Remove panes that are no longer selected, releasing their sockets.
        for id in currentIDs.subtracting(nextIDs) {
            if let child = panes.removeValue(forKey: id) {
                if let item = tabView.tabViewItems.first(where: { ($0.viewController as? AppObservationViewController) === child }) {
                    tabView.removeTabViewItem(item)
                }
                child.teardown()
                child.removeFromParent()
            }
        }

        // Add panes for newly selected apps.
        for id in next.appIDs where panes[id] == nil {
            let child = AppObservationViewController(
                appID: id,
                apiClient: apiClient,
                registry: registry
            )
            panes[id] = child
            addChild(child)
            let item = NSTabViewItem(viewController: child)
            item.label = id
            tabView.addTabViewItem(item)
        }

        // Reorder tabs to match the plan order.
        reorderTabs(to: next.appIDs)

        plan = next
        reflectEmptyState()
        return next
    }

    // MARK: - Private

    private func reorderTabs(to ids: [String]) {
        for (index, id) in ids.enumerated() {
            guard let child = panes[id],
                  let item = tabView.tabViewItems.first(where: { ($0.viewController as? AppObservationViewController) === child })
            else { continue }
            let currentIndex = tabView.indexOfTabViewItem(item)
            if currentIndex != index && currentIndex != NSNotFound {
                tabView.removeTabViewItem(item)
                tabView.insertTabViewItem(item, at: index)
            }
        }
    }

    private func reflectEmptyState() {
        let empty = plan.appIDs.isEmpty
        tabView.isHidden = empty
        emptyLabel.isHidden = !empty
    }
}
