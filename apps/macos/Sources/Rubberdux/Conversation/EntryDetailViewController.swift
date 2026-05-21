import AppKit

final class EntryDetailViewController: NSViewController {
    private let textView = NSTextView()
    private let scrollView = NSScrollView()

    override func loadView() {
        view = NSView()

        scrollView.documentView = textView
        scrollView.hasVerticalScroller = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false

        textView.isEditable = false
        textView.isSelectable = true
        textView.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        textView.textContainerInset = NSSize(width: 12, height: 12)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.textContainer?.widthTracksTextView = true
        textView.autoresizingMask = [.width]

        view.addSubview(scrollView)
        NSLayoutConstraint.activate([
            scrollView.topAnchor.constraint(equalTo: view.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            scrollView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])
    }

    func display(_ entry: Entry, entries: [Entry]) {
        var lines: [String] = []

        switch entry.message {
        case .system(let content):
            lines.append("--- System ---")
            lines.append(content)

        case .user(let content):
            lines.append("--- User ---")
            switch content {
            case .text(let text):
                lines.append(text)
            case .parts(let parts):
                for part in parts {
                    switch part {
                    case .text(let text):
                        lines.append(text)
                    case .imageUrl(let url):
                        lines.append("[Image: \(url)]")
                    case .videoUrl(let url):
                        lines.append("[Video: \(url)]")
                    }
                }
            }

        case .assistant(let content, let reasoning, let toolCalls, let partial):
            lines.append("--- Assistant\(partial == true ? " (partial)" : "") ---")
            if let content, !content.isEmpty {
                lines.append(content)
            }
            if let reasoning, !reasoning.isEmpty {
                lines.append("")
                lines.append("[Reasoning]")
                lines.append(reasoning)
            }
            if let toolCalls, !toolCalls.isEmpty {
                for call in toolCalls {
                    lines.append("")
                    lines.append("[Tool Call] \(call.function.name)")
                    lines.append("Call ID: \(call.id)")
                    lines.append("Arguments:")
                    lines.append(Self.prettyPrintJSON(call.function.arguments))
                }
            }

        case .tool(let toolCallId, let name, let content):
            lines.append("--- Tool Result ---")
            if let name {
                lines.append("Name: \(name)")
            }
            lines.append("Call ID: \(toolCallId)")

            // Cross-reference: find the parent assistant entry and the matching tool call
            if let parentId = entry.parentId,
               let parentEntry = entries.first(where: { $0.id == parentId }),
               case .assistant(_, _, let toolCalls, _) = parentEntry.message,
               let toolCalls,
               let matchingCall = toolCalls.first(where: { $0.id == toolCallId })
            {
                lines.append("")
                lines.append("[Original Tool Call] \(matchingCall.function.name)")
                lines.append("Arguments:")
                lines.append(Self.prettyPrintJSON(matchingCall.function.arguments))
            }

            lines.append("")
            lines.append("[Result]")
            lines.append(content)
        }

        textView.string = lines.joined(separator: "\n")
    }

    private static func prettyPrintJSON(_ jsonString: String) -> String {
        guard let data = jsonString.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let pretty = try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys]),
              let result = String(data: pretty, encoding: .utf8)
        else {
            return jsonString
        }
        return result
    }
}
