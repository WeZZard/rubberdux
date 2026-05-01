import AppKit

class PromptViewController: NSViewController {
    private let textView = NSTextView()
    private let scrollView = NSScrollView()
    private let kind: PromptKind
    private let apiClient: APIClient

    init(kind: PromptKind, apiClient: APIClient) {
        self.kind = kind
        self.apiClient = apiClient
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        view = NSView()

        scrollView.translatesAutoresizingMaskIntoConstraints = false
        scrollView.hasVerticalScroller = true
        scrollView.documentView = textView
        view.addSubview(scrollView)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: view.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])

        textView.isEditable = false
        textView.isRichText = false
        textView.font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
        textView.textContainerInset = NSSize(width: 12, height: 12)
        textView.autoresizingMask = [.width]
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        refresh()
    }

    func refresh() {
        Task {
            do {
                let content: String
                switch kind {
                case .system: content = try await apiClient.systemPrompt()
                case .identity: content = try await apiClient.identityPrompt()
                case .soul: content = try await apiClient.soulPrompt()
                }
                await MainActor.run {
                    self.textView.string = content
                }
            } catch {
                // Log or show error
            }
        }
    }
}
