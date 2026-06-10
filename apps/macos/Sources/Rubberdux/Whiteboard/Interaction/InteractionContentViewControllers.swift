import AppKit

// MARK: - InteractionResponder

/// The callback an interaction content view controller invokes when the human
/// answers. The owning controller (popover or pending list) forwards the
/// response to the backend and clears the interaction from its pending state.
typealias InteractionResponder = (InteractionResponse) -> Void

// MARK: - InteractionContentViewController

/// Factory that maps one `AgentInteraction` to the small `NSViewController` that
/// renders it. There is one view-controller class per primitive, each rendering
/// purely from the request's data — the single interaction vocabulary
/// (Approval / Question / Choice / Preview) is customized through the data it
/// carries, never through bespoke per-app view code.
///
/// See `docs/apps/macos/whiteboard-interaction.md` for the interaction-surface
/// design record.
enum InteractionContentViewController {

    /// Build the content view controller that renders `interaction`. `onRespond`
    /// is invoked with the human's answer; the caller owns delivery to the
    /// backend.
    static func make(
        for interaction: AgentInteraction,
        onRespond: @escaping InteractionResponder
    ) -> NSViewController {
        switch interaction {
        case let .approval(requestId, _, flavor, prompt):
            return ApprovalInteractionViewController(
                requestId: requestId,
                flavor: flavor,
                prompt: prompt,
                onRespond: onRespond
            )
        case let .question(requestId, _, text, options):
            return QuestionInteractionViewController(
                requestId: requestId,
                text: text,
                options: options,
                onRespond: onRespond
            )
        case let .choice(requestId, _, prompt, options):
            return ChoiceInteractionViewController(
                requestId: requestId,
                prompt: prompt,
                options: options,
                onRespond: onRespond
            )
        case let .preview(requestId, _, prompt, artifact):
            return PreviewInteractionViewController(
                requestId: requestId,
                prompt: prompt,
                artifact: artifact,
                onRespond: onRespond
            )
        }
    }
}

// MARK: - Shared layout

/// The fixed content width every interaction surface uses, so the popover and
/// the pending list size identically regardless of the primitive shown.
private enum InteractionLayout {
    static let width: CGFloat = 300
    static let margin: CGFloat = 14
    static let spacing: CGFloat = 10
}

/// A vertical stack pre-configured for an interaction surface: pinned to the
/// content width with uniform margins. Subviews are added by each primitive.
private func makeInteractionStack() -> NSStackView {
    let stack = NSStackView()
    stack.orientation = .vertical
    stack.alignment = .leading
    stack.spacing = InteractionLayout.spacing
    stack.translatesAutoresizingMaskIntoConstraints = false
    stack.edgeInsets = NSEdgeInsets(
        top: InteractionLayout.margin,
        left: InteractionLayout.margin,
        bottom: InteractionLayout.margin,
        right: InteractionLayout.margin
    )
    return stack
}

/// Wrap a configured stack into the view controller's root view, constraining the
/// content width so the popover sizes to a consistent column.
private func installStack(_ stack: NSStackView, into controller: NSViewController) {
    let container = NSView()
    container.addSubview(stack)
    NSLayoutConstraint.activate([
        stack.topAnchor.constraint(equalTo: container.topAnchor),
        stack.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
        stack.trailingAnchor.constraint(equalTo: container.trailingAnchor),
        stack.widthAnchor.constraint(equalToConstant: InteractionLayout.width),
    ])
    controller.view = container
}

/// A wrapping prompt label sized to the interaction column width.
private func makePromptLabel(_ text: String) -> NSTextField {
    let label = NSTextField(wrappingLabelWithString: text)
    label.font = .systemFont(ofSize: NSFont.systemFontSize)
    label.translatesAutoresizingMaskIntoConstraints = false
    label.preferredMaxLayoutWidth = InteractionLayout.width - 2 * InteractionLayout.margin
    return label
}

// MARK: - ApprovalInteractionViewController

/// Renders an `approval` interaction: the prompt plus Approve / Decline. The
/// flavor (permission vs. plan) is carried back verbatim in the response so the
/// backend can distinguish a permission grant from a plan sign-off.
final class ApprovalInteractionViewController: NSViewController {

    private let requestId: String
    private let flavor: ApprovalFlavor
    private let prompt: String
    private let onRespond: InteractionResponder

    init(
        requestId: String,
        flavor: ApprovalFlavor,
        prompt: String,
        onRespond: @escaping InteractionResponder
    ) {
        self.requestId = requestId
        self.flavor = flavor
        self.prompt = prompt
        self.onRespond = onRespond
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let stack = makeInteractionStack()

        let title = NSTextField(labelWithString: flavor == .plan ? "Plan approval" : "Permission")
        title.font = .boldSystemFont(ofSize: NSFont.smallSystemFontSize)
        title.textColor = .secondaryLabelColor
        stack.addArrangedSubview(title)

        stack.addArrangedSubview(makePromptLabel(prompt))

        let buttons = NSStackView()
        buttons.orientation = .horizontal
        buttons.spacing = 8

        let decline = NSButton(title: "Decline", target: self, action: #selector(declineTapped))
        decline.bezelStyle = .rounded
        let approve = NSButton(title: "Approve", target: self, action: #selector(approveTapped))
        approve.bezelStyle = .rounded
        approve.keyEquivalent = "\r"
        buttons.addArrangedSubview(decline)
        buttons.addArrangedSubview(approve)
        stack.addArrangedSubview(buttons)

        installStack(stack, into: self)
    }

    @objc private func approveTapped() {
        onRespond(.approved(requestId: requestId, flavor: flavor))
    }

    @objc private func declineTapped() {
        onRespond(.declined(requestId: requestId, flavor: flavor, reason: ""))
    }
}

// MARK: - QuestionInteractionViewController

/// Renders a `question` interaction: the question text plus one button per
/// option, answered with the selected option's index.
final class QuestionInteractionViewController: NSViewController {

    private let requestId: String
    private let text: String
    private let options: [ChoiceOption]
    private let onRespond: InteractionResponder

    init(
        requestId: String,
        text: String,
        options: [ChoiceOption],
        onRespond: @escaping InteractionResponder
    ) {
        self.requestId = requestId
        self.text = text
        self.options = options
        self.onRespond = onRespond
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let stack = makeInteractionStack()
        stack.addArrangedSubview(makePromptLabel(text))
        appendOptionButtons(options, to: stack, target: self, action: #selector(optionTapped(_:)))
        installStack(stack, into: self)
    }

    @objc private func optionTapped(_ sender: NSButton) {
        onRespond(.answered(requestId: requestId, selected: sender.tag, reply: nil))
    }
}

// MARK: - ChoiceInteractionViewController

/// Renders a `choice` interaction: a prompt plus one button per option,
/// answered with the selected option's index. Shares the `answered` response
/// with `question`; the two differ only in framing.
final class ChoiceInteractionViewController: NSViewController {

    private let requestId: String
    private let prompt: String
    private let options: [ChoiceOption]
    private let onRespond: InteractionResponder

    init(
        requestId: String,
        prompt: String,
        options: [ChoiceOption],
        onRespond: @escaping InteractionResponder
    ) {
        self.requestId = requestId
        self.prompt = prompt
        self.options = options
        self.onRespond = onRespond
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let stack = makeInteractionStack()
        stack.addArrangedSubview(makePromptLabel(prompt))
        appendOptionButtons(options, to: stack, target: self, action: #selector(optionTapped(_:)))
        installStack(stack, into: self)
    }

    @objc private func optionTapped(_ sender: NSButton) {
        onRespond(.answered(requestId: requestId, selected: sender.tag, reply: nil))
    }
}

// MARK: - PreviewInteractionViewController

/// Renders a `preview` interaction: a prompt, the generated artifact's text,
/// and a single Acknowledge button. The artifact is shown verbatim in a
/// scrollable text view; non-text MIME types fall back to their raw content.
final class PreviewInteractionViewController: NSViewController {

    private let requestId: String
    private let prompt: String
    private let artifact: PreviewArtifact
    private let onRespond: InteractionResponder

    init(
        requestId: String,
        prompt: String,
        artifact: PreviewArtifact,
        onRespond: @escaping InteractionResponder
    ) {
        self.requestId = requestId
        self.prompt = prompt
        self.artifact = artifact
        self.onRespond = onRespond
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let stack = makeInteractionStack()
        stack.addArrangedSubview(makePromptLabel(prompt))

        let mime = NSTextField(labelWithString: artifact.mimeType)
        mime.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        mime.textColor = .secondaryLabelColor
        stack.addArrangedSubview(mime)

        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false
        let textView = NSTextView()
        textView.isEditable = false
        textView.string = artifact.content
        textView.font = .monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        scroll.documentView = textView
        stack.addArrangedSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.heightAnchor.constraint(equalToConstant: 140),
            scroll.widthAnchor.constraint(
                equalToConstant: InteractionLayout.width - 2 * InteractionLayout.margin
            ),
        ])

        let acknowledge = NSButton(title: "Acknowledge", target: self, action: #selector(acknowledgeTapped))
        acknowledge.bezelStyle = .rounded
        acknowledge.keyEquivalent = "\r"
        stack.addArrangedSubview(acknowledge)

        installStack(stack, into: self)
    }

    @objc private func acknowledgeTapped() {
        onRespond(.acknowledged(requestId: requestId))
    }
}

// MARK: - Option button helper

/// Append one full-width push button per option to `stack`. Each button carries
/// its option index as its `tag`, so the handler reports the selected index in
/// the `answered` response. The option's label is the button title; its
/// description, when present, becomes the tooltip.
private func appendOptionButtons(
    _ options: [ChoiceOption],
    to stack: NSStackView,
    target: AnyObject,
    action: Selector
) {
    for (index, option) in options.enumerated() {
        let button = NSButton(title: option.label, target: target, action: action)
        button.bezelStyle = .rounded
        button.tag = index
        if !option.description.isEmpty {
            button.toolTip = option.description
        }
        stack.addArrangedSubview(button)
    }
}
