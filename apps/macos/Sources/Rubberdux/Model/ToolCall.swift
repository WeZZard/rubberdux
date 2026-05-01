import Foundation

struct ToolCall: Codable, Identifiable {
    let index: Int?
    let id: String
    let type: String
    let function: FunctionCall
    let dependsOn: String?

    enum CodingKeys: String, CodingKey {
        case index, id
        case type = "type"
        case function
        case dependsOn = "depends_on"
    }
}

struct FunctionCall: Codable {
    let name: String
    let arguments: String
}
