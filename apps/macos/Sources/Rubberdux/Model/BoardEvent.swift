import Foundation

// MARK: - BoardEvent

/// A board-level lifecycle event emitted over the board WebSocket stream.
/// Mirrors the `BoardEvent` enum in `src/app/supervisor.rs`.
/// The `kind` field is the discriminator.
enum BoardEvent: Codable, Equatable {
    /// A new App was created on the board.
    case created(app: App)
    /// An existing App's status changed.
    case statusChanged(id: String, status: AppStatus)
    /// An App's board position changed.
    case moved(id: String, position: BoardPosition)
    /// An App was archived and removed from the default board listing.
    case archived(id: String)

    // MARK: Codable

    private enum KindKey: String, CodingKey {
        case kind
    }

    private enum CreatedKeys: String, CodingKey {
        case kind, app
    }

    private enum StatusChangedKeys: String, CodingKey {
        case kind, id, status
    }

    private enum MovedKeys: String, CodingKey {
        case kind, id, position
    }

    private enum ArchivedKeys: String, CodingKey {
        case kind, id
    }

    init(from decoder: Decoder) throws {
        let kindContainer = try decoder.container(keyedBy: KindKey.self)
        let kind = try kindContainer.decode(String.self, forKey: .kind)

        switch kind {
        case "created":
            let c = try decoder.container(keyedBy: CreatedKeys.self)
            self = .created(app: try c.decode(App.self, forKey: .app))

        case "status_changed":
            let c = try decoder.container(keyedBy: StatusChangedKeys.self)
            self = .statusChanged(
                id: try c.decode(String.self, forKey: .id),
                status: try c.decode(AppStatus.self, forKey: .status)
            )

        case "moved":
            let c = try decoder.container(keyedBy: MovedKeys.self)
            self = .moved(
                id: try c.decode(String.self, forKey: .id),
                position: try c.decode(BoardPosition.self, forKey: .position)
            )

        case "archived":
            let c = try decoder.container(keyedBy: ArchivedKeys.self)
            self = .archived(id: try c.decode(String.self, forKey: .id))

        default:
            throw DecodingError.dataCorruptedError(
                forKey: KindKey.kind,
                in: kindContainer,
                debugDescription: "Unknown BoardEvent kind: \(kind)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .created(let app):
            var c = encoder.container(keyedBy: CreatedKeys.self)
            try c.encode("created", forKey: .kind)
            try c.encode(app, forKey: .app)

        case .statusChanged(let id, let status):
            var c = encoder.container(keyedBy: StatusChangedKeys.self)
            try c.encode("status_changed", forKey: .kind)
            try c.encode(id, forKey: .id)
            try c.encode(status, forKey: .status)

        case .moved(let id, let position):
            var c = encoder.container(keyedBy: MovedKeys.self)
            try c.encode("moved", forKey: .kind)
            try c.encode(id, forKey: .id)
            try c.encode(position, forKey: .position)

        case .archived(let id):
            var c = encoder.container(keyedBy: ArchivedKeys.self)
            try c.encode("archived", forKey: .kind)
            try c.encode(id, forKey: .id)
        }
    }
}
