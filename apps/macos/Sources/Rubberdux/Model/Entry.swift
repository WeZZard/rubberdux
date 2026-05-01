import Foundation

struct Entry: Codable, Identifiable {
    let id: Int
    let parentId: Int?
    let message: Message

    enum CodingKeys: String, CodingKey {
        case id
        case parentId = "parent_id"
        case message
    }
}
