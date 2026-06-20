import AppKit

// MARK: - CreateAppController

/// A small task-entry popover anchored at the empty cell the user clicked. It
/// collects a one-line task description and reports it back through `onSubmit`;
/// the whiteboard view controller owns the network call and the optimistic
/// placeholder, so this controller stays free of app/session logic and only
/// drives text entry.
///
/// See `docs/apps/macos/whiteboard-client.md` for the whiteboard client design
/// record.
final class CreateAppController: NSViewController, NSTextFieldDelegate {

    private enum Outcome {
        case submitted(String)
        case cancelled
    }

    // MARK: - Callbacks

    /// Called with the trimmed task text when the user commits (Return or the
    /// Create button). The popover closes after this fires.
    var onSubmit: ((String) -> Void)?

    /// Called when the user dismisses without submitting (Escape or click-out).
    var onCancel: (() -> Void)?

    // MARK: - Subviews

    private let textField = NSTextField()
    private let createButton = NSButton()
    private let popover = NSPopover()
    private var outcome: Outcome?

    // MARK: - Lifecycle

    override func loadView() {
        let label = NSTextField(labelWithString: "New task")
        label.font = .boldSystemFont(ofSize: NSFont.smallSystemFontSize)
        label.textColor = .secondaryLabelColor

        textField.placeholderString = "What should this app do?"
        textField.delegate = self

        createButton.title = "Create"
        createButton.bezelStyle = .rounded
        createButton.keyEquivalent = "\r"
        createButton.target = self
        createButton.action = #selector(commit)

        // Wrap the button in a trailing-aligned stack so NSStackView fills the
        // button to its intrinsic size rather than stretching it full-width.
        let buttonRow = NSStackView(views: [NSView(), createButton])
        buttonRow.orientation = .horizontal
        buttonRow.distribution = .gravityAreas
        buttonRow.setHuggingPriority(.defaultLow, for: .horizontal)

        let stack = NSStackView(views: [label, textField, buttonRow])
        stack.orientation = .vertical
        stack.spacing = 8
        stack.edgeInsets = NSEdgeInsets(top: 12, left: 12, bottom: 12, right: 12)
        stack.translatesAutoresizingMaskIntoConstraints = false

        let container = NSView()
        container.addSubview(stack)

        NSLayoutConstraint.activate([
            container.widthAnchor.constraint(equalToConstant: 310),
            stack.topAnchor.constraint(equalTo: container.topAnchor),
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            stack.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])

        view = container
        preferredContentSize = container.fittingSize
    }

    // MARK: - Presentation

    /// Present the entry popover anchored at `rect` within `view`, on the cell
    /// the user clicked. The text field takes first responder immediately.
    func present(relativeTo rect: NSRect, of view: NSView) {
        popover.contentViewController = self
        popover.behavior = .transient
        popover.delegate = self
        popover.show(relativeTo: rect, of: view, preferredEdge: .maxY)
        view.window?.makeFirstResponder(textField)
    }

    /// Close the popover. The close notification resolves cancellation unless
    /// an outcome was already recorded.
    func close() {
        popover.performClose(nil)
    }

    // MARK: - Actions

    @objc func commit() {
        let task = textField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !task.isEmpty else { return }
        resolve(as: .submitted(task))
        close()
    }

    private func resolve(as outcome: Outcome) {
        guard self.outcome == nil else { return }
        self.outcome = outcome

        switch outcome {
        case let .submitted(task):
            onSubmit?(task)
        case .cancelled:
            onCancel?()
        }

        onSubmit = nil
        onCancel = nil
    }

    // MARK: - NSTextFieldDelegate

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(insertNewline(_:)) {
            commit()
            return true
        }
        if selector == #selector(NSResponder.cancelOperation(_:)) {
            resolve(as: .cancelled)
            close()
            return true
        }
        return false
    }
}

// MARK: - NSPopoverDelegate

extension CreateAppController: NSPopoverDelegate {
    func popoverDidClose(_ notification: Notification) {
        // NSPopover reports both click-out and programmatic closes here. Treat
        // every non-detach close as cancellation unless another exit already
        // resolved first.
        if let reason = notification.userInfo?[NSPopover.closeReasonUserInfoKey] as? NSPopover.CloseReason,
           reason == .detachToWindow {
            return
        }
        resolve(as: .cancelled)
        popover.contentViewController = nil
    }
}
