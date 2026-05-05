import Foundation

struct Entry: Codable, Identifiable {
    let id: Int
    let parentId: Int?
    let message: Message
    let origin: EntryOrigin
    let channelMetadata: JSONValue?

    enum CodingKeys: String, CodingKey {
        case id
        case parentId = "parent_id"
        case message
        case origin
        case channelMetadata = "channel_metadata"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(Int.self, forKey: .id)
        parentId = try container.decodeIfPresent(Int.self, forKey: .parentId)
        message = try container.decode(Message.self, forKey: .message)
        origin = try container.decodeIfPresent(EntryOrigin.self, forKey: .origin) ?? .system
        channelMetadata = try container.decodeIfPresent(JSONValue.self, forKey: .channelMetadata)
    }
}

// MARK: - EntryOrigin

enum EntryOrigin: Codable, Equatable {
    case system
    case assistant
    case toolCall
    case user(channel: String)

    private enum TypeKey: String, CodingKey {
        case type
    }

    private enum UserKeys: String, CodingKey {
        case type, channel
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: TypeKey.self)
        let type_ = try container.decode(String.self, forKey: .type)

        switch type_ {
        case "system":
            self = .system
        case "assistant":
            self = .assistant
        case "tool_call":
            self = .toolCall
        case "user":
            let userContainer = try decoder.container(keyedBy: UserKeys.self)
            let channel = try userContainer.decode(String.self, forKey: .channel)
            self = .user(channel: channel)
        default:
            self = .system
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .system:
            var container = encoder.container(keyedBy: TypeKey.self)
            try container.encode("system", forKey: .type)
        case .assistant:
            var container = encoder.container(keyedBy: TypeKey.self)
            try container.encode("assistant", forKey: .type)
        case .toolCall:
            var container = encoder.container(keyedBy: TypeKey.self)
            try container.encode("tool_call", forKey: .type)
        case .user(let channel):
            var container = encoder.container(keyedBy: UserKeys.self)
            try container.encode("user", forKey: .type)
            try container.encode(channel, forKey: .channel)
        }
    }
}
