import AppKit

class MainSplitViewController: NSSplitViewController, ConversationViewDelegate {
    private let sidebarViewController = SidebarViewController()
    private let apiClient: APIClient
    private let webSocketClient: WebSocketClient
    private var contentControllers: [SidebarItem: NSViewController] = [:]
    private let contentContainer = ContentContainerViewController()
    private let entryDetailViewController = EntryDetailViewController()
    private var inspectorItem: NSSplitViewItem!

    init(baseURL: URL) {
        self.apiClient = APIClient(baseURL: baseURL)
        self.webSocketClient = WebSocketClient(baseURL: baseURL)
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()

        let sidebarItem = NSSplitViewItem(sidebarWithViewController: sidebarViewController)
        sidebarItem.minimumThickness = 200
        sidebarItem.maximumThickness = 300
        addSplitViewItem(sidebarItem)

        contentContainer.show(contentController(for: .conversation))
        let contentItem = NSSplitViewItem(viewController: contentContainer)
        addSplitViewItem(contentItem)

        inspectorItem = NSSplitViewItem(inspectorWithViewController: entryDetailViewController)
        inspectorItem.minimumThickness = 200
        inspectorItem.maximumThickness = 400
        inspectorItem.isCollapsed = true
        addSplitViewItem(inspectorItem)

        sidebarViewController.onSelectionChanged = { [weak self] item in
            self?.showContent(for: item)
        }

        sidebarViewController.onSettingsClicked = {
            (NSApp.delegate as? AppDelegate)?.showSettings()
        }

        webSocketClient.connect()
    }

    private func showContent(for item: SidebarItem) {
        contentContainer.show(contentController(for: item))
    }

    private func contentController(for item: SidebarItem) -> NSViewController {
        if let cached = contentControllers[item] {
            return cached
        }
        let vc: NSViewController
        switch item {
        case .conversation:
            let conversationVC = ConversationViewController(apiClient: apiClient, webSocketClient: webSocketClient)
            conversationVC.delegate = self
            vc = conversationVC
        case .identityPrompt:
            vc = PromptViewController(kind: .identity, apiClient: apiClient)
        case .soulPrompt:
            vc = PromptViewController(kind: .soul, apiClient: apiClient)
        case .systemPrompts:
            vc = PromptViewController(kind: .system, apiClient: apiClient)
        case .liveEvents:
            vc = EventStreamViewController(apiClient: apiClient, webSocketClient: webSocketClient)
        case .rawTrajectory:
            vc = EventStreamViewController(apiClient: apiClient, webSocketClient: webSocketClient)
        }
        contentControllers[item] = vc
        return vc
    }

    // MARK: - ConversationViewDelegate

    func conversationView(_ controller: ConversationViewController, didSelectEntry entry: Entry, entries: [Entry]) {
        entryDetailViewController.display(entry, entries: entries)
        inspectorItem.animator().isCollapsed = false
    }
}

/// Hosts a single child view controller and swaps it in place. NSSplitViewItem
/// disallows replacing its viewController once added to a SplitViewController,
/// so we keep the SplitViewItem stable and swap the child here.
private final class ContentContainerViewController: NSViewController {
    private weak var currentChild: NSViewController?

    override func loadView() {
        view = NSView()
    }

    func show(_ child: NSViewController) {
        if currentChild === child { return }

        if let previous = currentChild {
            previous.view.removeFromSuperview()
            previous.removeFromParent()
        }

        addChild(child)
        child.view.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(child.view)
        NSLayoutConstraint.activate([
            child.view.topAnchor.constraint(equalTo: view.topAnchor),
            child.view.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            child.view.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            child.view.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])
        currentChild = child
    }
}
