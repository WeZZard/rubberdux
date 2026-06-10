import AppKit
import Combine

// MARK: - AppObservationViewController

/// The observation pane for a single App: a split of its conversation (top) over
/// its trajectory (bottom), plus a debug message box that posts a free-form task
/// to the App via `POST /api/v1/apps/{id}/tasks`.
///
/// Both tables snapshot from the per-app REST endpoints
/// (`entries(appID:)` / `trajectory(appID:)`) and then follow the per-app live
/// stream obtained from `AppSocketRegistry`, reusing the row/cell rendering of
/// `ConversationViewController` / `EventStreamViewController`. The pane balances
/// its `registry.open`/`registry.close` so leaving the selection never leaks the
/// socket.
final class AppObservationViewController: NSViewController, NSTextFieldDelegate {

    // MARK: - Collaborators

    let appID: String
    private let apiClient: APIClient
    private let registry: AppSocketRegistry

    private var socket: AppSocket?
    private var cancellables: Set<AnyCancellable> = []

    private lazy var conversation = AppConversationViewController(appID: appID, apiClient: apiClient)
    private lazy var trajectory = AppTrajectoryViewController(appID: appID, apiClient: apiClient)

    private let debugField = NSTextField()

    // MARK: - Init

    init(appID: String, apiClient: APIClient, registry: AppSocketRegistry) {
        self.appID = appID
        self.apiClient = apiClient
        self.registry = registry
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: - Lifecycle

    override func loadView() {
        let container = NSView()

        let split = NSSplitView()
        split.translatesAutoresizingMaskIntoConstraints = false
        split.isVertical = false
        split.dividerStyle = .thin
        addChild(conversation)
        addChild(trajectory)
        split.addArrangedSubview(conversation.view)
        split.addArrangedSubview(trajectory.view)
        container.addSubview(split)

        debugField.placeholderString = "Send a debug task…"
        debugField.translatesAutoresizingMaskIntoConstraints = false
        debugField.delegate = self
        container.addSubview(debugField)

        NSLayoutConstraint.activate([
            split.topAnchor.constraint(equalTo: container.topAnchor),
            split.bottomAnchor.constraint(equalTo: debugField.topAnchor, constant: -8),
            split.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            split.trailingAnchor.constraint(equalTo: container.trailingAnchor),

            debugField.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -8),
            debugField.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 8),
            debugField.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -8),
        ])

        view = container
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        connect()
    }

    // MARK: - Streams

    /// Open the per-app socket and route its live entries/trajectory into the two
    /// child tables.
    private func connect() {
        let socket = registry.open(appID: appID)
        self.socket = socket

        socket.entrySubject
            .receive(on: RunLoop.main)
            .sink { [weak self] notification in
                self?.conversation.append(notification.entry)
            }
            .store(in: &cancellables)

        socket.trajectorySubject
            .receive(on: RunLoop.main)
            .sink { [weak self] event in
                self?.trajectory.append(event)
            }
            .store(in: &cancellables)
    }

    /// Release the per-app socket. The owner calls this before removing the pane,
    /// so the registry ref-count drops and the underlying connection closes when
    /// no other pane references the App.
    func teardown() {
        cancellables.removeAll()
        if socket != nil {
            registry.close(appID: appID)
            socket = nil
        }
    }

    deinit {
        // Safety net: if `teardown` was not called, still balance the open.
        if socket != nil {
            registry.close(appID: appID)
        }
    }

    // MARK: - Debug message box

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        guard commandSelector == #selector(insertNewline(_:)) else { return false }
        let text = debugField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return true }
        debugField.stringValue = ""
        let appID = appID
        Task { @MainActor in
            do {
                try await apiClient.sendTask(appID: appID, text: text)
            } catch {
                NSLog("[Observation] Failed to send debug task for %@: %@", appID, "\(error)")
            }
        }
        return true
    }
}

// MARK: - AppConversationViewController

/// Per-app conversation table. Reuses the role/content column layout and cell
/// rendering of `ConversationViewController`, bound to the App's own entry
/// endpoint and live stream rather than the single-agent global stream.
final class AppConversationViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    private let tableView = NSTableView()
    private let scrollView = NSScrollView()
    private var entries: [Entry] = []

    private let appID: String
    private let apiClient: APIClient

    init(appID: String, apiClient: APIClient) {
        self.appID = appID
        self.apiClient = apiClient
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        view = NSView()
        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scrollView)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: view.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])

        let roleColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("role"))
        roleColumn.title = "Role"
        roleColumn.width = 80
        tableView.addTableColumn(roleColumn)

        let contentColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("content"))
        contentColumn.title = "Content"
        tableView.addTableColumn(contentColumn)

        tableView.dataSource = self
        tableView.delegate = self
        tableView.usesAlternatingRowBackgroundColors = true
        tableView.rowSizeStyle = .custom
        tableView.rowHeight = 44
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let appID = appID
        Task { @MainActor in
            if let fetched = try? await apiClient.entries(appID: appID) {
                self.entries = fetched
                self.tableView.reloadData()
            }
        }
    }

    /// Append a live entry, deduplicating by id, and scroll it into view.
    func append(_ entry: Entry) {
        guard !entries.contains(where: { $0.id == entry.id }) else { return }
        entries.append(entry)
        tableView.reloadData()
        tableView.scrollRowToVisible(entries.count - 1)
    }

    func numberOfRows(in tableView: NSTableView) -> Int { entries.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let entry = entries[row]
        let cellView = NSTableCellView()
        let textField = NSTextField(labelWithString: "")
        textField.translatesAutoresizingMaskIntoConstraints = false
        textField.lineBreakMode = .byTruncatingTail
        textField.maximumNumberOfLines = 1
        cellView.addSubview(textField)
        cellView.textField = textField
        NSLayoutConstraint.activate([
            textField.leadingAnchor.constraint(equalTo: cellView.leadingAnchor, constant: 4),
            textField.trailingAnchor.constraint(equalTo: cellView.trailingAnchor, constant: -4),
            textField.centerYAnchor.constraint(equalTo: cellView.centerYAnchor),
        ])

        switch tableColumn?.identifier.rawValue {
        case "role":
            textField.stringValue = entry.message.role
            textField.font = NSFont.boldSystemFont(ofSize: NSFont.systemFontSize)
            switch entry.message.role {
            case "system": textField.textColor = .systemGray
            case "user": textField.textColor = .systemBlue
            case "assistant": textField.textColor = .systemGreen
            case "tool": textField.textColor = .systemOrange
            default: break
            }
        case "content":
            textField.stringValue = entry.message.contentText.replacingOccurrences(of: "\n", with: "\\n")
        default:
            break
        }
        return cellView
    }
}

// MARK: - AppTrajectoryViewController

/// Per-app trajectory table. Reuses the type/timestamp/agent column layout and
/// cell rendering of `EventStreamViewController`, bound to the App's own
/// trajectory endpoint and live stream.
final class AppTrajectoryViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    private let tableView = NSTableView()
    private let scrollView = NSScrollView()
    private var events: [TrajectoryEvent] = []

    private let appID: String
    private let apiClient: APIClient

    init(appID: String, apiClient: APIClient) {
        self.appID = appID
        self.apiClient = apiClient
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        view = NSView()
        scrollView.documentView = tableView
        scrollView.hasVerticalScroller = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scrollView)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: view.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])

        let typeColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("type"))
        typeColumn.title = "Event Type"
        typeColumn.width = 200
        tableView.addTableColumn(typeColumn)

        let timestampColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("timestamp"))
        timestampColumn.title = "Timestamp"
        timestampColumn.width = 200
        tableView.addTableColumn(timestampColumn)

        let agentColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("agent"))
        agentColumn.title = "Agent"
        agentColumn.width = 100
        tableView.addTableColumn(agentColumn)

        tableView.dataSource = self
        tableView.delegate = self
        tableView.usesAlternatingRowBackgroundColors = true
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let appID = appID
        Task { @MainActor in
            if let fetched = try? await apiClient.trajectory(appID: appID) {
                self.events = fetched
                self.tableView.reloadData()
            }
        }
    }

    /// Append a live trajectory event and scroll it into view.
    func append(_ event: TrajectoryEvent) {
        events.append(event)
        tableView.reloadData()
        tableView.scrollRowToVisible(events.count - 1)
    }

    func numberOfRows(in tableView: NSTableView) -> Int { events.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let event = events[row]
        let cellView = NSTableCellView()
        let textField = NSTextField(labelWithString: "")
        textField.translatesAutoresizingMaskIntoConstraints = false
        cellView.addSubview(textField)
        cellView.textField = textField
        NSLayoutConstraint.activate([
            textField.leadingAnchor.constraint(equalTo: cellView.leadingAnchor, constant: 4),
            textField.trailingAnchor.constraint(equalTo: cellView.trailingAnchor, constant: -4),
            textField.centerYAnchor.constraint(equalTo: cellView.centerYAnchor),
        ])

        switch tableColumn?.identifier.rawValue {
        case "type": textField.stringValue = event.type
        case "timestamp": textField.stringValue = event.timestamp
        case "agent": textField.stringValue = event.agentId
        default: break
        }
        return cellView
    }
}
