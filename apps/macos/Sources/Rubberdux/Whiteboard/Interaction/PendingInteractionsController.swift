import AppKit

// MARK: - PendingInteractionsController

/// The pop-out list of an App's pending interactions, presented from its icon
/// badge when the board window is not front. Each pending interaction is shown
/// stacked vertically in its own content view controller (the same per-primitive
/// vocabulary the popover uses), so the human can answer any of them in place.
/// Answering forwards the response through `onRespond`; the owner delivers it to
/// the backend and refreshes the list.
///
/// See `docs/apps/macos/whiteboard-interaction.md` for the interaction-surface
/// design record.
final class PendingInteractionsController: NSViewController {

    // MARK: - Callbacks

    /// Forwarded the human's answer to one of the listed interactions. The owner
    /// delivers it to the backend and updates the pending model.
    var onRespond: InteractionResponder?

    // MARK: - State

    private var interactions: [AgentInteraction] = []
    private let stack = NSStackView()
    private let popover = NSPopover()
    private var childControllers: [NSViewController] = []

    // MARK: - Lifecycle

    override func loadView() {
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 8
        stack.translatesAutoresizingMaskIntoConstraints = false

        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.translatesAutoresizingMaskIntoConstraints = false

        let documentView = NSView()
        documentView.translatesAutoresizingMaskIntoConstraints = false
        documentView.addSubview(stack)
        scroll.documentView = documentView

        let container = NSView()
        container.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: container.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: container.trailingAnchor),

            stack.topAnchor.constraint(equalTo: documentView.topAnchor),
            stack.bottomAnchor.constraint(equalTo: documentView.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: documentView.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: documentView.trailingAnchor),
            stack.widthAnchor.constraint(equalTo: documentView.widthAnchor),

            container.widthAnchor.constraint(equalToConstant: 320),
            container.heightAnchor.constraint(lessThanOrEqualToConstant: 480),
        ])

        view = container
        rebuild()
    }

    // MARK: - Content

    /// Replace the listed interactions and rebuild the stacked content. Empty
    /// input shows a placeholder so the pop-out is never blank.
    func setInteractions(_ interactions: [AgentInteraction]) {
        self.interactions = interactions
        if isViewLoaded {
            rebuild()
        }
    }

    /// Present the pending list anchored to `rect` within `boardView`, on the
    /// trailing edge of the badged icon.
    func present(relativeTo rect: NSRect, of boardView: NSView) {
        popover.behavior = .transient
        popover.contentViewController = self
        popover.show(relativeTo: rect, of: boardView, preferredEdge: .maxX)
    }

    /// Dismiss the pop-out.
    func dismiss() {
        popover.performClose(nil)
    }

    // MARK: - Private

    private func rebuild() {
        for child in childControllers {
            child.removeFromParent()
        }
        childControllers.removeAll()
        for arranged in stack.arrangedSubviews {
            stack.removeArrangedSubview(arranged)
            arranged.removeFromSuperview()
        }

        guard !interactions.isEmpty else {
            let placeholder = NSTextField(labelWithString: "No pending interactions.")
            placeholder.textColor = .secondaryLabelColor
            stack.addArrangedSubview(placeholder)
            return
        }

        for interaction in interactions {
            let child = InteractionContentViewController.make(for: interaction) { [weak self] response in
                self?.onRespond?(response)
            }
            addChild(child)
            childControllers.append(child)
            stack.addArrangedSubview(child.view)
        }
    }
}
