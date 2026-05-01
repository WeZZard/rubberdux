import Foundation

struct TrajectoryEvent: Codable, Identifiable {
    let schemaVersion: Int
    let eventId: String
    let seq: UInt64
    let timestamp: String
    let type: String
    let source: String
    let sessionId: String?
    let agentId: String
    let turnId: String?
    let taskId: String?
    let toolCallId: String?
    let operationId: String?
    let causedBy: String?
    let actor: String?
    let subject: String?
    let payload: JSONValue

    var id: String { eventId }

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case eventId = "event_id"
        case seq, timestamp
        case type = "type"
        case source
        case sessionId = "session_id"
        case agentId = "agent_id"
        case turnId = "turn_id"
        case taskId = "task_id"
        case toolCallId = "tool_call_id"
        case operationId = "operation_id"
        case causedBy = "caused_by"
        case actor, subject, payload
    }
}

// MARK: - JSONValue

/// A type-erased JSON value that mirrors `serde_json::Value`.
enum JSONValue: Codable, Equatable {
    case null
    case bool(Bool)
    case int(Int)
    case double(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()

        if container.decodeNil() {
            self = .null
            return
        }
        if let value = try? container.decode(Bool.self) {
            self = .bool(value)
            return
        }
        if let value = try? container.decode(Int.self) {
            self = .int(value)
            return
        }
        if let value = try? container.decode(Double.self) {
            self = .double(value)
            return
        }
        if let value = try? container.decode(String.self) {
            self = .string(value)
            return
        }
        if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
            return
        }
        if let value = try? container.decode([String: JSONValue].self) {
            self = .object(value)
            return
        }
        throw DecodingError.dataCorruptedError(
            in: container,
            debugDescription: "Cannot decode JSONValue"
        )
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null:
            try container.encodeNil()
        case .bool(let value):
            try container.encode(value)
        case .int(let value):
            try container.encode(value)
        case .double(let value):
            try container.encode(value)
        case .string(let value):
            try container.encode(value)
        case .array(let value):
            try container.encode(value)
        case .object(let value):
            try container.encode(value)
        }
    }
}
