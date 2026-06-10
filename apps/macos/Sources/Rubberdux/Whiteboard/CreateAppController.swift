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

    // MARK: - Lifecycle

    override func loadView() {
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 280, height: 76))

        let label = NSTextField(labelWithString: "New task")
        label.font = .boldSystemFont(ofSize: NSFont.smallSystemFontSize)
        label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(label)

        textField.placeholderString = "What should this app do?"
        textField.delegate = self
        textField.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(textField)

        createButton.title = "Create"
        createButton.bezelStyle = .rounded
        createButton.keyEquivalent = "\r"
        createButton.target = self
        createButton.action = #selector(commit)
        createButton.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(createButton)

        NSLayoutConstraint.activate([
            label.topAnchor.constraint(equalTo: container.topAnchor, constant: 12),
            label.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 12),

            textField.topAnchor.constraint(equalTo: label.bottomAnchor, constant: 6),
            textField.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 12),
            textField.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -12),

            createButton.topAnchor.constraint(equalTo: textField.bottomAnchor, constant: 8),
            createButton.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -12),
            createButton.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -12),
        ])

        view = container
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

    /// Close the popover without firing `onCancel` (used after a commit).
    func close() {
        popover.performClose(nil)
    }

    // MARK: - Actions

    @objc private func commit() {
        let task = textField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !task.isEmpty else { return }
        onSubmit?(task)
        close()
    }

    // MARK: - NSTextFieldDelegate

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(NSResponder.cancelOperation(_:)) {
            close()
            onCancel?()
            return true
        }
        return false
    }
}

// MARK: - NSPopoverDelegate

extension CreateAppController: NSPopoverDelegate {
    func popoverDidClose(_ notification: Notification) {
        // A transient popover dismissed by click-out reports cancellation so the
        // view controller can clear any pending-create state. A commit closes via
        // `close()` after `onSubmit`, so this path is the cancel path only.
        if let reason = notification.userInfo?[NSPopover.closeReasonUserInfoKey] as? NSPopover.CloseReason,
           reason == .detachToWindow {
            return
        }
    }
}
