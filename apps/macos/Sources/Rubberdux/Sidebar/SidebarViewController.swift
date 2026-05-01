import AppKit

class SidebarViewController: NSViewController, NSOutlineViewDataSource, NSOutlineViewDelegate {
    var onSelectionChanged: ((SidebarItem) -> Void)?
    var onSettingsClicked: (() -> Void)?

    private let outlineView = NSOutlineView()
    private let scrollView = NSScrollView()

    override func loadView() {
        view = NSView()

        scrollView.documentView = outlineView
        scrollView.hasVerticalScroller = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(scrollView)

        let separator = NSBox()
        separator.boxType = .separator
        separator.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(separator)

        let settingsButton = NSButton(
            image: NSImage(systemSymbolName: "gearshape", accessibilityDescription: "Settings")!,
            target: self,
            action: #selector(settingsButtonClicked)
        )
        settingsButton.bezelStyle = .accessoryBarAction
        settingsButton.isBordered = false
        settingsButton.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(settingsButton)

        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: view.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: separator.topAnchor),
            scrollView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: view.trailingAnchor),

            separator.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            separator.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            separator.bottomAnchor.constraint(equalTo: settingsButton.topAnchor, constant: -4),

            settingsButton.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 8),
            settingsButton.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -4),
        ])

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("sidebar"))
        column.title = ""
        outlineView.addTableColumn(column)
        outlineView.outlineTableColumn = column
        outlineView.headerView = nil
        outlineView.dataSource = self
        outlineView.delegate = self
        outlineView.style = .sourceList
        outlineView.rowSizeStyle = .default
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        outlineView.reloadData()
        // Expand headers
        for item in SidebarItem.topLevel {
            if item.children != nil {
                outlineView.expandItem(item)
            }
        }
    }

    // MARK: - NSOutlineViewDataSource

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        guard let item = item as? SidebarItem else {
            return SidebarItem.topLevel.count
        }
        return item.children?.count ?? 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        guard let item = item as? SidebarItem else {
            return SidebarItem.topLevel[index]
        }
        return item.children![index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        guard let item = item as? SidebarItem else { return false }
        return item.children != nil
    }

    // MARK: - NSOutlineViewDelegate

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let sidebarItem = item as? SidebarItem else { return nil }
        let cellView = NSTableCellView()
        let textField = NSTextField(labelWithString: sidebarItem.description)
        textField.translatesAutoresizingMaskIntoConstraints = false
        cellView.addSubview(textField)
        cellView.textField = textField
        NSLayoutConstraint.activate([
            textField.leadingAnchor.constraint(equalTo: cellView.leadingAnchor, constant: 4),
            textField.centerYAnchor.constraint(equalTo: cellView.centerYAnchor),
        ])

        if sidebarItem.isHeader {
            textField.font = NSFont.boldSystemFont(ofSize: NSFont.smallSystemFontSize)
            textField.textColor = .secondaryLabelColor
        }

        return cellView
    }

    func outlineView(_ outlineView: NSOutlineView, isGroupItem item: Any) -> Bool {
        guard let sidebarItem = item as? SidebarItem else { return false }
        return sidebarItem.isHeader
    }

    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        guard let sidebarItem = item as? SidebarItem else { return false }
        return !sidebarItem.isHeader
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        let row = outlineView.selectedRow
        guard row >= 0, let item = outlineView.item(atRow: row) as? SidebarItem else { return }
        onSelectionChanged?(item)
    }

    @objc private func settingsButtonClicked() {
        onSettingsClicked?()
    }
}
