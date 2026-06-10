import Foundation

// MARK: - BoardPosition

/// A tile's location on the whiteboard grid. Mirrors `BoardPositionDto`.
struct BoardPosition: Codable, Equatable {
    let row: Int
    let column: Int
}

// MARK: - Icon

/// An app icon: an SF-Symbol name and a named color. Mirrors `IconDto`.
struct Icon: Codable, Equatable {
    let symbol: String
    let color: String
}

// MARK: - AppStatus

/// The lifecycle state of an App. Mirrors the `status` string field on `AppDto`.
enum AppStatus: String, Codable, Equatable {
    case active
    case tombstoned
}

// MARK: - App

/// A persistent agent app as represented on the whiteboard board.
/// Mirrors `AppDto` in `src/gateway/apps.rs`.
struct App: Codable, Identifiable, Equatable {
    let id: String
    let title: String
    let icon: Icon
    let position: BoardPosition
    let status: AppStatus
    let summary: String
    let userLocked: Bool
    let lastActive: String

    enum CodingKeys: String, CodingKey {
        case id
        case title
        case icon
        case position
        case status
        case summary
        case userLocked = "user_locked"
        case lastActive = "last_active"
    }
}
