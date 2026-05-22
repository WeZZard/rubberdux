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
        let defaultFont = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        let defaultAttrs: [NSAttributedString.Key: Any] = [.font: defaultFont]
        let output = NSMutableAttributedString()

        func appendLine(_ text: String) {
            output.append(NSAttributedString(string: text + "\n", attributes: defaultAttrs))
        }

        func appendImage(from url: String) {
            if url.hasPrefix("data:"),
               let commaIndex = url.firstIndex(of: ","),
               let data = Data(base64Encoded: String(url[url.index(after: commaIndex)...])),
               let image = NSImage(data: data)
            {
                let attachment = NSTextAttachment()
                attachment.image = image
                let maxWidth: CGFloat = 400
                let scale = min(1.0, maxWidth / image.size.width)
                attachment.bounds = CGRect(
                    x: 0, y: 0,
                    width: image.size.width * scale,
                    height: image.size.height * scale
                )
                output.append(NSAttributedString(attachment: attachment))
                output.append(NSAttributedString(string: "\n", attributes: defaultAttrs))
            } else {
                appendLine("[Image: \(url)]")
            }
        }

        switch entry.message {
        case .system(let content):
            appendLine("--- System ---")
            appendLine(content)

        case .user(let content):
            appendLine("--- User ---")
            switch content {
            case .text(let text):
                appendLine(text)
            case .parts(let parts):
                for part in parts {
                    switch part {
                    case .text(let text):
                        appendLine(text)
                    case .imageUrl(let url):
                        appendImage(from: url)
                    case .videoUrl(let url):
                        appendLine("[Video: \(url)]")
                    }
                }
            }

        case .assistant(let content, let reasoning, let toolCalls, let partial):
            appendLine("--- Assistant\(partial == true ? " (partial)" : "") ---")
            if let content, !content.isEmpty {
                appendLine(content)
            }
            if let reasoning, !reasoning.isEmpty {
                appendLine("")
                appendLine("[Reasoning]")
                appendLine(reasoning)
            }
            if let toolCalls, !toolCalls.isEmpty {
                for call in toolCalls {
                    appendLine("")
                    appendLine("[Tool Call] \(call.function.name)")
                    appendLine("Call ID: \(call.id)")
                    appendLine("Arguments:")
                    appendLine(Self.prettyPrintJSON(call.function.arguments))
                }
            }

        case .tool(let toolCallId, let name, let content):
            appendLine("--- Tool Result ---")
            if let name {
                appendLine("Name: \(name)")
            }
            appendLine("Call ID: \(toolCallId)")

            // Cross-reference: find the parent assistant entry and the matching tool call
            if let parentId = entry.parentId,
               let parentEntry = entries.first(where: { $0.id == parentId }),
               case .assistant(_, _, let toolCalls, _) = parentEntry.message,
               let toolCalls,
               let matchingCall = toolCalls.first(where: { $0.id == toolCallId })
            {
                appendLine("")
                appendLine("[Original Tool Call] \(matchingCall.function.name)")
                appendLine("Arguments:")
                appendLine(Self.prettyPrintJSON(matchingCall.function.arguments))
            }

            appendLine("")
            appendLine("[Result]")
            appendLine(content)
        }

        textView.textStorage?.setAttributedString(output)
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
