import AppKit
import Combine

class ConversationViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate {
    private let tableView = NSTableView()
    private let scrollView = NSScrollView()
    private let inputField = NSTextField()
    private var entries: [Entry] = []
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

        inputField.placeholderString = "Type a message..."
        inputField.translatesAutoresizingMaskIntoConstraints = false
        inputField.delegate = self
        view.addSubview(inputField)

        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: view.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: inputField.topAnchor, constant: -8),
            scrollView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: view.trailingAnchor),

            inputField.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -8),
            inputField.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 8),
            inputField.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -8),
        ])

        let roleColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("role"))
        roleColumn.title = "Role"
        roleColumn.width = 80
        tableView.addTableColumn(roleColumn)

        let contentColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("content"))
        contentColumn.title = "Content"
        tableView.addTableColumn(contentColumn)

        let idColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("id"))
        idColumn.title = "ID"
        idColumn.width = 40
        tableView.addTableColumn(idColumn)

        tableView.dataSource = self
        tableView.delegate = self
        tableView.usesAlternatingRowBackgroundColors = true
        tableView.rowSizeStyle = .custom
        tableView.rowHeight = 44
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        refresh()

        webSocketClient.chatEntrySubject
            .receive(on: DispatchQueue.main)
            .sink { [weak self] notification in
                guard let self else { return }
                if !self.entries.contains(where: { $0.id == notification.entry.id }) {
                    self.entries.append(notification.entry)
                    self.tableView.reloadData()
                    self.tableView.scrollRowToVisible(self.entries.count - 1)
                }
            }
            .store(in: &cancellables)
    }

    // MARK: - NSTextFieldDelegate

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(insertNewline(_:)) {
            let text = inputField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !text.isEmpty else { return true }
            webSocketClient.sendMessage(text)
            inputField.stringValue = ""
            return true
        }
        return false
    }

    func refresh() {
        Task {
            do {
                let fetched = try await apiClient.entries()
                await MainActor.run {
                    self.entries = fetched
                    self.tableView.reloadData()
                }
            } catch {
                // Log or show error
            }
        }
    }

    // MARK: - NSTableViewDataSource

    func numberOfRows(in tableView: NSTableView) -> Int {
        entries.count
    }

    // MARK: - NSTableViewDelegate

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let entry = entries[row]
        let cellView = NSTableCellView()
        let textField = NSTextField(labelWithString: "")
        textField.translatesAutoresizingMaskIntoConstraints = false
        textField.lineBreakMode = .byTruncatingTail
        cellView.addSubview(textField)
        cellView.textField = textField
        NSLayoutConstraint.activate([
            textField.leadingAnchor.constraint(equalTo: cellView.leadingAnchor, constant: 4),
            textField.trailingAnchor.constraint(equalTo: cellView.trailingAnchor, constant: -4),
            textField.centerYAnchor.constraint(equalTo: cellView.centerYAnchor),
        ])

        guard let columnId = tableColumn?.identifier.rawValue else { return cellView }

        switch columnId {
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
            textField.stringValue = entry.message.contentText
        case "id":
            textField.stringValue = "\(entry.id)"
            textField.alignment = .center
        default:
            break
        }

        return cellView
    }
}
