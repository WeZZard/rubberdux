import AppKit

class MainSplitViewController: NSSplitViewController {
    private let sidebarViewController = SidebarViewController()
    private let apiClient: APIClient
    private let webSocketClient: WebSocketClient

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

        let contentItem = NSSplitViewItem(viewController: ConversationViewController(apiClient: apiClient))
        addSplitViewItem(contentItem)

        let inspectorItem = NSSplitViewItem(inspectorWithViewController: NSViewController())
        inspectorItem.minimumThickness = 200
        inspectorItem.maximumThickness = 400
        inspectorItem.isCollapsed = true
        addSplitViewItem(inspectorItem)

        sidebarViewController.onSelectionChanged = { [weak self] item in
            self?.showContent(for: item)
        }

        webSocketClient.connect()
    }

    private func showContent(for item: SidebarItem) {
        let vc: NSViewController
        switch item {
        case .conversation:
            vc = ConversationViewController(apiClient: apiClient)
        case .identityPrompt:
            vc = PromptViewController(kind: .identity, apiClient: apiClient)
        case .soulPrompt:
            vc = PromptViewController(kind: .soul, apiClient: apiClient)
        case .systemPrompts:
            vc = PromptViewController(kind: .system, apiClient: apiClient)
        case .liveEvents:
            vc = EventStreamViewController(webSocketClient: webSocketClient)
        case .rawTrajectory:
            vc = EventStreamViewController(webSocketClient: webSocketClient)
        }

        splitViewItems[1].viewController = vc
    }
}
