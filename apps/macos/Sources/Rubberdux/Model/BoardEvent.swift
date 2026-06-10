import Foundation

// MARK: - BoardEvent

/// A board-level frame emitted over the board WebSocket stream
/// (`/api/v1/ws/board`). Mirrors `BoardWsMessage` in
/// `src/gateway/apps_stream.rs`, which uses
/// `#[serde(tag = "type", rename_all = "snake_case")]`. The `type` field is the
/// wire discriminator, decoupled from the Rust variant names.
enum BoardEvent: Codable, Equatable {
    /// A new App appeared on the board (`"app_created"`).
    case appCreated(app: App)
    /// An existing App's status or position changed (`"updated"`).
    case updated(id: String)
    /// An App was archived and left the board (`"archived"`).
    case archived(id: String)
    /// The number of interactions an App is awaiting changed (`"badge"`).
    case badge(appId: String, count: Int)

    // MARK: Codable

    private enum TypeKey: String, CodingKey {
        case type
    }

    private enum AppCreatedKeys: String, CodingKey {
        case type, app
    }

    private enum UpdatedKeys: String, CodingKey {
        case type, id
    }

    private enum ArchivedKeys: String, CodingKey {
        case type, id
    }

    private enum BadgeKeys: String, CodingKey {
        case type
        case appId = "app_id"
        case count
    }

    init(from decoder: Decoder) throws {
        let typeContainer = try decoder.container(keyedBy: TypeKey.self)
        let type = try typeContainer.decode(String.self, forKey: .type)

        switch type {
        case "app_created":
            let c = try decoder.container(keyedBy: AppCreatedKeys.self)
            self = .appCreated(app: try c.decode(App.self, forKey: .app))

        case "updated":
            let c = try decoder.container(keyedBy: UpdatedKeys.self)
            self = .updated(id: try c.decode(String.self, forKey: .id))

        case "archived":
            let c = try decoder.container(keyedBy: ArchivedKeys.self)
            self = .archived(id: try c.decode(String.self, forKey: .id))

        case "badge":
            let c = try decoder.container(keyedBy: BadgeKeys.self)
            self = .badge(
                appId: try c.decode(String.self, forKey: .appId),
                count: try c.decode(Int.self, forKey: .count)
            )

        default:
            throw DecodingError.dataCorruptedError(
                forKey: TypeKey.type,
                in: typeContainer,
                debugDescription: "Unknown BoardEvent type: \(type)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .appCreated(let app):
            var c = encoder.container(keyedBy: AppCreatedKeys.self)
            try c.encode("app_created", forKey: .type)
            try c.encode(app, forKey: .app)

        case .updated(let id):
            var c = encoder.container(keyedBy: UpdatedKeys.self)
            try c.encode("updated", forKey: .type)
            try c.encode(id, forKey: .id)

        case .archived(let id):
            var c = encoder.container(keyedBy: ArchivedKeys.self)
            try c.encode("archived", forKey: .type)
            try c.encode(id, forKey: .id)

        case .badge(let appId, let count):
            var c = encoder.container(keyedBy: BadgeKeys.self)
            try c.encode("badge", forKey: .type)
            try c.encode(appId, forKey: .appId)
            try c.encode(count, forKey: .count)
        }
    }
}
