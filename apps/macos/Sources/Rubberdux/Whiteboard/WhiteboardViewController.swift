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

        let container = NSView()
        container.addSubview(boardView)
        NSLayoutConstraint.activate([
            boardView.topAnchor.constraint(equalTo: container.topAnchor),
            boardView.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            boardView.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            boardView.trailingAnchor.constraint(equalTo: container.trailingAnchor),
        ])

        let recognizer = NSPanGestureRecognizer(target: self, action: #selector(handleDrag(_:)))
        boardView.addGestureRecognizer(recognizer)
        dragRecognizer = recognizer

        view = container
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        subscribeToBoard()
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
        case let .badge(appId, count):
            boardView.setBadge(count, forAppID: appId)
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
        case .unchanged:
            break
        }
    }

    private func reapplySelection() {
        boardView.selectApp(selectedAppIDs.first)
    }

    // MARK: - BoardViewDelegate

    func boardView(_ boardView: BoardView, didSelectAppID appID: String) {
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

    // MARK: - Logging

    private func log(_ message: String) {
        NSLog("[Whiteboard] %@", message)
    }
}
