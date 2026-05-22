import Foundation

// MARK: - Message

enum Message: Codable {
    case system(content: String)
    case user(content: UserContent)
    case assistant(
        content: String?,
        reasoningContent: String?,
        toolCalls: [ToolCall]?,
        partial: Bool?
    )
    case tool(toolCallId: String, name: String?, content: String)

    var role: String {
        switch self {
        case .system: return "system"
        case .user: return "user"
        case .assistant: return "assistant"
        case .tool: return "tool"
        }
    }

    /// The primary text content of the message, or an empty string when absent.
    var contentText: String {
        switch self {
        case .system(let content):
            return content
        case .user(let content):
            switch content {
            case .text(let text):
                return text
            case .parts(let parts):
                var texts: [String] = []
                var hasImage = false
                for part in parts {
                    switch part {
                    case .text(let text):
                        texts.append(text)
                    case .imageUrl:
                        hasImage = true
                    case .videoUrl:
                        hasImage = true
                    }
                }
                let textContent = texts.joined(separator: " ")
                if hasImage {
                    return textContent.isEmpty ? "[Image]" : "\(textContent) [Image]"
                }
                return textContent.isEmpty ? "" : textContent
            }
        case .assistant(let content, _, let toolCalls, _):
            if let text = content, !text.isEmpty {
                return text
            }
            if let calls = toolCalls, !calls.isEmpty {
                let names = calls.map { $0.function.name }.joined(separator: ", ")
                return "[tool calls: \(names)]"
            }
            return ""
        case .tool(_, _, let content):
            return content
        }
    }

    // MARK: Codable

    private enum RoleKey: String, CodingKey {
        case role
    }

    private enum SystemKeys: String, CodingKey {
        case role, content
    }

    private enum UserKeys: String, CodingKey {
        case role, content
    }

    private enum AssistantKeys: String, CodingKey {
        case role, content
        case reasoningContent = "reasoning_content"
        case toolCalls = "tool_calls"
        case partial
    }

    private enum ToolKeys: String, CodingKey {
        case role
        case toolCallId = "tool_call_id"
        case name, content
    }

    init(from decoder: Decoder) throws {
        let roleContainer = try decoder.container(keyedBy: RoleKey.self)
        let role = try roleContainer.decode(String.self, forKey: .role)

        switch role {
        case "system":
            let container = try decoder.container(keyedBy: SystemKeys.self)
            let content = try container.decode(String.self, forKey: .content)
            self = .system(content: content)

        case "user":
            let container = try decoder.container(keyedBy: UserKeys.self)
            let content = try container.decode(UserContent.self, forKey: .content)
            self = .user(content: content)

        case "assistant":
            let container = try decoder.container(keyedBy: AssistantKeys.self)
            let content = try container.decodeIfPresent(String.self, forKey: .content)
            let reasoningContent = try container.decodeIfPresent(
                String.self, forKey: .reasoningContent
            )
            let toolCalls = try container.decodeIfPresent(
                [ToolCall].self, forKey: .toolCalls
            )
            let partial = try container.decodeIfPresent(Bool.self, forKey: .partial)
            self = .assistant(
                content: content,
                reasoningContent: reasoningContent,
                toolCalls: toolCalls,
                partial: partial
            )

        case "tool":
            let container = try decoder.container(keyedBy: ToolKeys.self)
            let toolCallId = try container.decode(String.self, forKey: .toolCallId)
            let name = try container.decodeIfPresent(String.self, forKey: .name)
            let content = try container.decode(String.self, forKey: .content)
            self = .tool(toolCallId: toolCallId, name: name, content: content)

        default:
            throw DecodingError.dataCorruptedError(
                forKey: RoleKey.role,
                in: roleContainer,
                debugDescription: "Unknown role: \(role)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .system(let content):
            var container = encoder.container(keyedBy: SystemKeys.self)
            try container.encode("system", forKey: .role)
            try container.encode(content, forKey: .content)

        case .user(let content):
            var container = encoder.container(keyedBy: UserKeys.self)
            try container.encode("user", forKey: .role)
            try container.encode(content, forKey: .content)

        case .assistant(let content, let reasoningContent, let toolCalls, let partial):
            var container = encoder.container(keyedBy: AssistantKeys.self)
            try container.encode("assistant", forKey: .role)
            try container.encodeIfPresent(content, forKey: .content)
            try container.encodeIfPresent(reasoningContent, forKey: .reasoningContent)
            try container.encodeIfPresent(toolCalls, forKey: .toolCalls)
            try container.encodeIfPresent(partial, forKey: .partial)

        case .tool(let toolCallId, let name, let content):
            var container = encoder.container(keyedBy: ToolKeys.self)
            try container.encode("tool", forKey: .role)
            try container.encode(toolCallId, forKey: .toolCallId)
            try container.encodeIfPresent(name, forKey: .name)
            try container.encode(content, forKey: .content)
        }
    }
}

// MARK: - UserContent

enum UserContent: Codable {
    case text(String)
    case parts([ContentPart])

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let text = try? container.decode(String.self) {
            self = .text(text)
            return
        }
        let parts = try container.decode([ContentPart].self)
        self = .parts(parts)
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .text(let text):
            try container.encode(text)
        case .parts(let parts):
            try container.encode(parts)
        }
    }
}

// MARK: - ContentPart

enum ContentPart: Codable {
    case text(text: String)
    case imageUrl(url: String)
    case videoUrl(url: String)

    private enum TypeKey: String, CodingKey {
        case type
    }

    private enum TextKeys: String, CodingKey {
        case type, text
    }

    private enum ImageUrlKeys: String, CodingKey {
        case type
        case imageUrl = "image_url"
    }

    private enum VideoUrlKeys: String, CodingKey {
        case type
        case videoUrl = "video_url"
    }

    init(from decoder: Decoder) throws {
        let typeContainer = try decoder.container(keyedBy: TypeKey.self)
        let type_ = try typeContainer.decode(String.self, forKey: .type)

        switch type_ {
        case "text":
            let container = try decoder.container(keyedBy: TextKeys.self)
            let text = try container.decode(String.self, forKey: .text)
            self = .text(text: text)

        case "image_url":
            let container = try decoder.container(keyedBy: ImageUrlKeys.self)
            let mediaUrl = try container.decode(MediaUrl.self, forKey: .imageUrl)
            self = .imageUrl(url: mediaUrl.url)

        case "video_url":
            let container = try decoder.container(keyedBy: VideoUrlKeys.self)
            let mediaUrl = try container.decode(MediaUrl.self, forKey: .videoUrl)
            self = .videoUrl(url: mediaUrl.url)

        default:
            throw DecodingError.dataCorruptedError(
                forKey: TypeKey.type,
                in: typeContainer,
                debugDescription: "Unknown content part type: \(type_)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .text(let text):
            var container = encoder.container(keyedBy: TextKeys.self)
            try container.encode("text", forKey: .type)
            try container.encode(text, forKey: .text)

        case .imageUrl(let url):
            var container = encoder.container(keyedBy: ImageUrlKeys.self)
            try container.encode("image_url", forKey: .type)
            try container.encode(MediaUrl(url: url), forKey: .imageUrl)

        case .videoUrl(let url):
            var container = encoder.container(keyedBy: VideoUrlKeys.self)
            try container.encode("video_url", forKey: .type)
            try container.encode(MediaUrl(url: url), forKey: .videoUrl)
        }
    }
}

// MARK: - MediaUrl

struct MediaUrl: Codable {
    let url: String
}
