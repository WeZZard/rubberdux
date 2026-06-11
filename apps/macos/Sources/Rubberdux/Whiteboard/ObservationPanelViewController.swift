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

    // MARK: - State

    /// The currently displayed plan, used to diff against incoming selections.
    private(set) var plan = ObservationSelectionPlan(selection: [], order: [])

    /// The per-App column controller, keyed by app id.
    private var columns: [String: ObservationColumn] = [:]

    private let splitView = NSSplitView()
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

        splitView.translatesAutoresizingMaskIntoConstraints = false
        splitView.isVertical = true
        splitView.dividerStyle = .thin
        container.addSubview(splitView)

        emptyLabel.translatesAutoresizingMaskIntoConstraints = false
        emptyLabel.textColor = .secondaryLabelColor
        emptyLabel.alignment = .center
        container.addSubview(emptyLabel)

        NSLayoutConstraint.activate([
            splitView.topAnchor.constraint(equalTo: container.topAnchor, constant: 8),
            splitView.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -8),
            splitView.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 8),
            splitView.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -8),

            emptyLabel.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            emptyLabel.centerYAnchor.constraint(equalTo: container.centerYAnchor),
        ])

        view = container
        reflectEmptyState()
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
