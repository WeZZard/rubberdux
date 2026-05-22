import AppKit
import Combine

class EventStreamViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    private let tableView = NSTableView()
    private let scrollView = NSScrollView()
    private var events: [TrajectoryEvent] = []
    private var cancellables = Set<AnyCancellable>()
    private let apiClient: APIClient
    private let webSocketClient: WebSocketClient

    init(apiClient: APIClient, webSocketClient: WebSocketClient) {
        self.apiClient = apiClient
        self.webSocketClient = webSocketClient
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

        // Fetch history from REST (cold source - events.jsonl)
        Task {
            do {
                let fetched = try await apiClient.trajectoryEvents()
                await MainActor.run {
                    self.events = fetched
                    self.tableView.reloadData()
                }
            } catch {
                // REST failed, will rely on WebSocket only
            }
        }

        // Subscribe for live updates (hot signal)
        webSocketClient.trajectorySubject
            .receive(on: DispatchQueue.main)
            .sink { [weak self] event in
                guard let self else { return }
                self.events.append(event)
                self.tableView.reloadData()
                if !self.events.isEmpty {
                    self.tableView.scrollRowToVisible(self.events.count - 1)
                }
            }
            .store(in: &cancellables)
    }

    // MARK: - NSTableViewDataSource

    func numberOfRows(in tableView: NSTableView) -> Int { events.count }

    // MARK: - NSTableViewDelegate

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
