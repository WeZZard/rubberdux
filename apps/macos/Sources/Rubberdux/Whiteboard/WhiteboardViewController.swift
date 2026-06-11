import AppKit
import Combine

// MARK: - WhiteboardViewController

/// The primary content view controller: it hosts the dot-grid `BoardView`,
/// drives it from live data, and owns the board's interaction flows.
///
/// On load it fetches the board with `APIClient.apps()`, seeds a `BoardAppStore`,
/// and renders icons through `BoardView`. It then subscribes to `/ws/board` via
/// `BoardSocket` and applies each `BoardEvent` to the store, resyncing the board
/// view. Clicking an empty cell opens a `CreateAppController` task popover; on
/// submit it inserts an optimistic placeholder icon, POSTs `/apps`, and collapses
/// the placeholder into the reconciled App by id. Clicking an icon selects it
/// (modifier-click extends the selection). Dragging an icon to a new cell
/// persists the move with `PATCH /apps/{id}`.
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
final class WhiteboardViewController: NSViewController, BoardViewDelegate {

    // MARK: - Collaborators

    private let apiClient: APIClient
    private let boardSocket: BoardSocket
    private let store = BoardAppStore()

    private lazy var boardView = BoardView(geometry: BoardGeometry(pitch: 64, origin: CGPoint(x: 80, y: 80)))

    private var cancellables: Set<AnyCancellable> = []

    // MARK: - Observation panel

    /// Shared, ref-counted per-app stream registry handed to the observation
    /// panel so its panes open/close per-app sockets without leaking.
    private lazy var socketRegistry = AppSocketRegistry(baseURL: apiClient.baseURL)

    /// The floating observation panel. It streams the selected apps' conversation
    /// and trajectory as a translucent card pinned to the board's right edge; it
    /// is hidden (board reclaims full width) when the selection is empty and shown
    /// (board reserves its width) when one or more apps are selected.
    private lazy var observationPanel = ObservationPanelViewController(
        apiClient: apiClient,
        registry: socketRegistry
    )

    // MARK: - Selection

    /// The currently selected app ids. The board view paints a single selection
    /// ring; this set tracks the multi-select model for callers that need it
    /// (the observation panel, a later task). The primary ring follows the most
    /// recent selection.
    private(set) var selectedAppIDs: Set<String> = []

    // MARK: - Create flow

    /// The in-flight create popover, retained while presented.
    private var createController: CreateAppController?

    // MARK: - Drag

    private var dragRecognizer: NSPanGestureRecognizer?
    private var draggingAppID: String?

    // MARK: - Interaction surfaces

    /// The pending interactions every shown App is awaiting. Drives both the icon
    /// badge counts and each selected App's inline pending list in its
    /// observation column.
    private var pendingInteractions = PendingInteractionsStore()

    /// One open interaction socket per shown App, keyed by app id. Opened when an
    /// App appears on the board and closed when it leaves, so raised/resolved
    /// events reach the board without a manual selection.
    private var interactionSockets: [String: InteractionSocket] = [:]

    /// Per-socket subscription tokens, released alongside the socket.
    private var interactionCancellables: [String: AnyCancellable] = [:]

    /// The popover shown when an interaction is raised while the board window is
    /// front. Reused across raises.
    private let interactionPopover = InteractionPopoverController()

    /// Whether the board window is the front, key surface. When `true` a raised
    /// interaction is presented as a popover; when `false` it only updates the
    /// icon badge and the pending list. Tracked via window/app notifications.
    private var isBoardFront: Bool = false

    // MARK: - Init

    init(apiClient: APIClient, boardSocket: BoardSocket) {
        self.apiClient = apiClient
        self.boardSocket = boardSocket
        super.init(nibName: nil, bundle: nil)
    }

    /// Convenience initializer that builds the board socket from the API base URL,
    /// so the sidebar router can construct the controller with just an `APIClient`.
    convenience init(apiClient: APIClient) {
        self.init(
            apiClient: apiClient,
            boardSocket: BoardSocket(baseURL: apiClient.baseURL)
        )
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: - Lifecycle

    override func loadView() {
        boardView.delegate = self
        boardView.translatesAutoresizingMaskIntoConstraints = false

        let recognizer = NSPanGestureRecognizer(target: self, action: #selector(handleDrag(_:)))
        boardView.addGestureRecognizer(recognizer)
        dragRecognizer = recognizer

        // The board fills the window; the observation panel floats over it, pinned
        // to the top, bottom, and trailing edges. The board reserves a usable-area
        // inset equal to the panel's width (set in `applyObservationVisibility`) so
        // its content reflows to the panel's left rather than hiding under it.
        let container = NSView()
        container.addSubview(boardView)
        addChild(observationPanel)
        observationPanel.view.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(observationPanel.view)

        NSLayoutConstraint.activate([
            boardView.topAnchor.constraint(equalTo: container.topAnchor),
            boardView.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            boardView.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            boardView.trailingAnchor.constraint(equalTo: container.trailingAnchor),

            observationPanel.view.topAnchor.constraint(equalTo: container.topAnchor, constant: 12),
            observationPanel.view.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -12),
            observationPanel.view.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -12),
        ])

        // Mirror the panel's width into the board's reserved inset whenever the
        // user drags the panel's resize handle.
        observationPanel.onWidthChange = { [weak self] _ in
            self?.applyReservedInset()
        }

        view = container
        // Start hidden: nothing is selected on load, so the board owns the full
        // width with no reserved inset.
        applyObservationVisibility()
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        // Route answers to inline column interactions back through the same
        // delivery path as the popover, so a response from a column behaves
        // exactly like one from the raised-interaction popover.
        observationPanel.onRespond = { [weak self] appID, response in
            self?.respond(response, appID: appID)
        }
        subscribeToBoard()
        observeFrontSurface()
        loadApps()
    }

    // MARK: - Data loading

    private func loadApps() {
        Task { @MainActor in
            do {
                let apps = try await apiClient.apps()
                store.applySnapshot(apps)
                resyncBoard()
            } catch {
                log("Failed to load apps: \(error)")
            }
        }
    }

    private func subscribeToBoard() {
        boardSocket.eventSubject
            .receive(on: RunLoop.main)
            .sink { [weak self] event in
                self?.handle(event)
            }
            .store(in: &cancellables)
        boardSocket.connect()
    }

    /// Apply a live board event to the store and reflect it on the board view.
    private func handle(_ event: BoardEvent) {
        switch event {
        case .badge:
            // The per-App interaction socket (`InteractionSocket`) is the
            // authoritative source of badge counts, derived from
            // `PendingInteractionsStore`. The board-level badge frame is ignored
            // here so it never clobbers the precise per-App count.
            break
        case .updated(let id):
            // The event carries only an id; refetch the single App and upsert.
            refetchAndUpsert(id: id)
        default:
            let change = store.apply(event)
            applyChangeToBoard(change)
        }
    }

    private func refetchAndUpsert(id: String) {
        Task { @MainActor in
            do {
                let apps = try await apiClient.apps()
                guard let app = apps.first(where: { $0.id == id }) else {
                    applyChangeToBoard(store.remove(id: id))
                    return
                }
                applyChangeToBoard(store.upsert(app))
            } catch {
                log("Failed to refetch app \(id): \(error)")
            }
        }
    }

    // MARK: - Board sync

    /// Push the entire store to the board view. Used after a snapshot or a
    /// `replacedAll` change.
    private func resyncBoard() {
        boardView.setApps(store.apps)
        reapplySelection()
        reapplyBadges()
        // A full snapshot may have removed a selected App; keep the panel honest.
        if isViewLoaded {
            syncObservationPanel()
            reconcileInteractionSockets()
        }
    }

    /// Map a single store change onto the board view incrementally.
    private func applyChangeToBoard(_ change: BoardAppStoreChange) {
        switch change {
        case .replacedAll:
            resyncBoard()
        case .upserted, .removed:
            // BoardView's public surface reconciles from the full app set, so a
            // single insert/move/remove is applied by replaying the current
            // store contents; it diffs internally and only touches the affected
            // icon layer.
            boardView.setApps(store.apps)
            reapplySelection()
            reapplyBadges()
            // A removed App may be in the selection; let the panel prune and
            // collapse if needed.
            syncObservationPanel()
            reconcileInteractionSockets()
        case .unchanged:
            // Even when the store reports no observable App change, the id-to-id
            // mapping may have moved (e.g. `reconcileCreate` swapped an optimistic
            // placeholder for a server id whose App was already inserted by a
            // board event, yielding `.unchanged`). Reconcile sockets so the
            // server-id interactions socket opens; the reconciliation is
            // idempotent for already-subscribed ids.
            reconcileInteractionSockets()
        }
    }

    private func reapplySelection() {
        boardView.selectApp(selectedAppIDs.first)
    }

    // MARK: - Observation panel sync

    /// Reconcile the observation panel against the current selection and the
    /// board's app order, then collapse or expand its split pane. Drops any
    /// selected ids that no longer exist on the board so the panel never streams
    /// a removed App.
    private func syncObservationPanel() {
        let liveIDs = Set(store.apps.map(\.id))
        let pruned = selectedAppIDs.intersection(liveIDs)
        if pruned != selectedAppIDs {
            selectedAppIDs = pruned
        }
        observationPanel.update(
            selection: selectedAppIDs,
            order: store.apps.map(\.id)
        )
        // Seed each shown column's inline pending list from the current model so
        // selecting a badged App immediately shows its awaited interactions.
        for appID in selectedAppIDs {
            refreshColumnInteractions(forAppID: appID)
        }
        applyObservationVisibility()
    }

    /// Show the floating observation panel when the selection is non-empty and
    /// hide it otherwise, then reconcile the board's reserved inset so the board
    /// reflows to the panel's left when shown and reclaims the full width when
    /// hidden.
    private func applyObservationVisibility() {
        observationPanel.view.isHidden = selectedAppIDs.isEmpty
        applyReservedInset()
    }

    /// Set the board's right inset to the panel's current width while it is shown,
    /// or `0` while it is hidden, then animate the board's reflow. The board's
    /// lattice, tiles, and create hit-testing all honor this inset so nothing
    /// lives under the floating panel.
    private func applyReservedInset() {
        let shown = !selectedAppIDs.isEmpty
        // The reserved strip spans the panel width plus its trailing/leading gap
        // to the window edge, so board content clears the whole floating card.
        let inset = shown ? observationPanel.panelWidth + 24 : 0
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0.2
            context.allowsImplicitAnimation = true
            boardView.rightInset = inset
            boardView.layoutSubtreeIfNeeded()
        }
    }

    // MARK: - BoardViewDelegate

    func boardView(_ boardView: BoardView, didSelectAppID appID: String) {
        // A click on a badged icon selects the App, surfacing its observation
        // column, whose inline pending list shows the awaited interactions; the
        // badge no longer opens a separate pop-out.
        let extending = NSEvent.modifierFlags.contains(.command) || NSEvent.modifierFlags.contains(.shift)
        if extending {
            if selectedAppIDs.contains(appID) {
                selectedAppIDs.remove(appID)
            } else {
                selectedAppIDs.insert(appID)
            }
        } else {
            selectedAppIDs = [appID]
        }
        boardView.selectApp(selectedAppIDs.contains(appID) ? appID : selectedAppIDs.first)
        syncObservationPanel()
    }

    func boardView(_ boardView: BoardView, didActivateEmptyCell cell: GridCell) {
        presentCreate(at: cell)
    }

    // MARK: - Create flow

    private func presentCreate(at cell: GridCell) {
        let controller = CreateAppController()
        controller.onSubmit = { [weak self] task in
            self?.createApp(task: task, at: cell)
            self?.createController = nil
        }
        controller.onCancel = { [weak self] in
            self?.createController = nil
        }
        createController = controller

        let anchor = NSRect(
            x: boardView.bounds.midX,
            y: boardView.bounds.midY,
            width: 1,
            height: 1
        )
        controller.present(relativeTo: anchor, of: boardView)
    }

    private func createApp(task: String, at cell: GridCell) {
        let position = BoardPosition(row: cell.row, column: cell.column)
        let placeholderID = BoardAppStore.placeholderID()
        let placeholder = App(
            id: placeholderID,
            title: task,
            icon: Icon(symbol: "circle.dashed", color: "#9AA0A6"),
            position: position,
            status: .active,
            summary: task,
            userLocked: false,
            lastActive: ""
        )
        applyChangeToBoard(store.insertPlaceholder(placeholder))

        Task { @MainActor in
            do {
                let app = try await apiClient.createApp(task: task, position: position)
                applyChangeToBoard(store.reconcileCreate(placeholderID: placeholderID, with: app))
            } catch {
                // Reconciliation failed; drop the optimistic placeholder so the
                // board does not keep a phantom icon.
                applyChangeToBoard(store.remove(id: placeholderID))
                log("Failed to create app: \(error)")
            }
        }
    }

    // MARK: - Drag to move

    @objc private func handleDrag(_ recognizer: NSPanGestureRecognizer) {
        let point = recognizer.location(in: boardView)
        switch recognizer.state {
        case .began:
            if case let .icon(appID) = boardView.hit(at: point) {
                draggingAppID = appID
            } else {
                draggingAppID = nil
            }
        case .ended:
            guard let appID = draggingAppID, let app = store.app(id: appID) else { return }
            draggingAppID = nil
            let current = GridCell(row: app.position.row, column: app.position.column)
            let cell = dropCell(at: point, fallback: current)
            guard cell != current else { return }
            persistMove(appID: appID, to: cell)
        case .cancelled, .failed:
            draggingAppID = nil
        default:
            break
        }
    }

    /// Optimistically move the icon in the store, resync, then persist with
    /// `PATCH /apps/{id}`. On failure the server snapshot from a later board
    /// event corrects the position.
    private func persistMove(appID: String, to cell: GridCell) {
        guard let app = store.app(id: appID) else { return }
        let moved = App(
            id: app.id,
            title: app.title,
            icon: app.icon,
            position: BoardPosition(row: cell.row, column: cell.column),
            status: app.status,
            summary: app.summary,
            userLocked: app.userLocked,
            lastActive: app.lastActive
        )
        applyChangeToBoard(store.upsert(moved))

        Task { @MainActor in
            do {
                let updated = try await apiClient.patchApp(
                    id: appID,
                    position: BoardPosition(row: cell.row, column: cell.column)
                )
                applyChangeToBoard(store.upsert(updated))
            } catch {
                log("Failed to persist move for \(appID): \(error)")
            }
        }
    }

    /// The grid cell a drop at `point` targets. An empty cell is the target as
    /// reported by the board; a drop onto another icon falls back to `fallback`
    /// (the dragged app's current cell) so the move is treated as a no-op rather
    /// than colliding.
    private func dropCell(at point: CGPoint, fallback: GridCell) -> GridCell {
        if case let .emptyCell(cell) = boardView.hit(at: point) {
            return cell
        }
        return fallback
    }

    // MARK: - Interaction surfaces

    /// Open an interaction socket for every App now on the board and close
    /// sockets for Apps that have left, so raised/resolved events reach the board
    /// without requiring the App to be selected. Idempotent: an App that is
    /// already subscribed is left untouched.
    private func reconcileInteractionSockets() {
        // An optimistic placeholder id has no backing worker on the backend, so
        // its interactions socket would only draw a "no active worker" rejection;
        // `interactionSocketIDs(for:)` excludes placeholders, leaving real server
        // ids only. Once `reconcileCreate` swaps optimistic→server, the next
        // reconciliation (run after every board change) opens the server-id
        // socket. See `docs/apps/macos/whiteboard-client.md`.
        let liveIDs = BoardAppStore.interactionSocketIDs(for: store.apps)
        let currentIDs = Set(interactionSockets.keys)

        for id in currentIDs.subtracting(liveIDs) {
            interactionSockets.removeValue(forKey: id)?.disconnect()
            interactionCancellables.removeValue(forKey: id)
            pendingInteractions.replace(appID: id, with: [])
        }

        for id in liveIDs.subtracting(currentIDs) {
            let socket = InteractionSocket(appID: id, baseURL: apiClient.baseURL)
            interactionCancellables[id] = socket.eventSubject
                .receive(on: RunLoop.main)
                .sink { [weak self] event in
                    self?.handleInteractionEvent(event, appID: id)
                }
            interactionSockets[id] = socket
            socket.connect()
        }
    }

    /// Apply one live interaction event to the pending model and the board's
    /// surfaces: a raise lights the badge and (if the board is front) shows the
    /// popover; a resolve clears the badge and dismisses any surface showing it.
    private func handleInteractionEvent(_ event: InteractionSocketEvent, appID: String) {
        switch event {
        case let .raised(interaction):
            pendingInteractions.raise(interaction)
            boardView.setBadge(pendingInteractions.badgeCount(forAppID: appID), forAppID: appID)
            refreshColumnInteractions(forAppID: appID)
            if isBoardFront {
                presentInteractionPopover(interaction, appID: appID)
            }
        case let .resolved(requestId):
            pendingInteractions.resolve(appID: appID, requestId: requestId)
            boardView.setBadge(pendingInteractions.badgeCount(forAppID: appID), forAppID: appID)
            interactionPopover.dismiss(ifShowing: requestId)
            refreshColumnInteractions(forAppID: appID)
        }
    }

    /// Present the interaction popover anchored to `appID`'s icon. Responses are
    /// sent over the App's interaction socket (with a POST fallback) and clear the
    /// interaction optimistically.
    private func presentInteractionPopover(_ interaction: AgentInteraction, appID: String) {
        guard let rect = boardView.iconRect(forAppID: appID) else { return }
        interactionPopover.present(interaction, relativeTo: rect, of: boardView) { [weak self] response in
            self?.respond(response, appID: appID)
            self?.interactionPopover.dismiss()
        }
    }

    /// Push the latest pending interactions for `appID` into its observation
    /// column's inline list. A no-op when the App is not currently selected, so
    /// the panel only renders interactions for shown columns.
    private func refreshColumnInteractions(forAppID appID: String) {
        observationPanel.updateInteractions(
            pendingInteractions.interactions(forAppID: appID),
            forAppID: appID
        )
    }

    /// Deliver `response` to the backend over the App's interaction socket when
    /// open, otherwise via the REST fallback, and clear it from the pending model
    /// so the badge decrements and the request leaves the list. The authoritative
    /// `resolved` event from the backend confirms the same clearing.
    private func respond(_ response: InteractionResponse, appID: String) {
        if let socket = interactionSockets[appID] {
            socket.respond(response)
        } else {
            Task { @MainActor in
                do {
                    try await apiClient.respondToInteraction(
                        appID: appID,
                        requestID: response.requestId,
                        response: response
                    )
                } catch {
                    log("Failed to respond to interaction \(response.requestId): \(error)")
                }
            }
        }
        pendingInteractions.resolve(appID: appID, requestId: response.requestId)
        boardView.setBadge(pendingInteractions.badgeCount(forAppID: appID), forAppID: appID)
        refreshColumnInteractions(forAppID: appID)
    }

    /// Reapply badge counts from the pending model after a board resync, so a
    /// `setApps` that recreates icon layers does not lose their badges.
    private func reapplyBadges() {
        for app in store.apps {
            boardView.setBadge(pendingInteractions.badgeCount(forAppID: app.id), forAppID: app.id)
        }
    }

    // MARK: - Front-surface tracking

    /// Observe window key/main and app activation notifications so the board can
    /// choose the popover surface (board is front) versus the badge surface
    /// (board is not front) when an interaction is raised.
    private func observeFrontSurface() {
        let center = NotificationCenter.default
        for name in [NSWindow.didBecomeKeyNotification, NSWindow.didBecomeMainNotification] {
            center.publisher(for: name)
                .receive(on: RunLoop.main)
                .sink { [weak self] _ in self?.updateFrontSurface() }
                .store(in: &cancellables)
        }
        for name in [NSWindow.didResignKeyNotification, NSWindow.didResignMainNotification] {
            center.publisher(for: name)
                .receive(on: RunLoop.main)
                .sink { [weak self] _ in self?.updateFrontSurface() }
                .store(in: &cancellables)
        }
        center.publisher(for: NSApplication.didResignActiveNotification)
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.updateFrontSurface() }
            .store(in: &cancellables)
        center.publisher(for: NSApplication.didBecomeActiveNotification)
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.updateFrontSurface() }
            .store(in: &cancellables)
        updateFrontSurface()
    }

    /// Recompute whether the board window is the front, key surface.
    private func updateFrontSurface() {
        let window = view.window
        isBoardFront = NSApp.isActive && (window?.isKeyWindow == true || window?.isMainWindow == true)
    }

    // MARK: - Logging

    private func log(_ message: String) {
        NSLog("[Whiteboard] %@", message)
    }
}
