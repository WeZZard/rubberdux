import Foundation

final class APIClient {
    let baseURL: URL
    private let session: URLSession
    private let decoder: JSONDecoder

    init(baseURL: URL = URL(string: "http://localhost:19385")!) {
        self.baseURL = baseURL
        self.session = URLSession.shared
        self.decoder = JSONDecoder()
    }

    // MARK: - Entries

    func entries(role: String? = nil, sinceId: Int? = nil) async throws -> [Entry] {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/entries"),
            resolvingAgainstBaseURL: false
        )!
        var queryItems: [URLQueryItem] = []
        if let role { queryItems.append(URLQueryItem(name: "role", value: role)) }
        if let sinceId { queryItems.append(URLQueryItem(name: "since_id", value: String(sinceId))) }
        if !queryItems.isEmpty { components.queryItems = queryItems }

        let (data, _) = try await session.data(from: components.url!)
        let response = try decoder.decode(EntriesResponse.self, from: data)
        return response.entries
    }

    func entry(id: Int) async throws -> Entry {
        let url = baseURL.appendingPathComponent("/api/v1/entries/\(id)")
        let (data, _) = try await session.data(from: url)
        return try decoder.decode(Entry.self, from: data)
    }

    // MARK: - Tool Calls

    func toolCalls() async throws -> [ToolCallPair] {
        let url = baseURL.appendingPathComponent("/api/v1/tool-calls")
        let (data, _) = try await session.data(from: url)
        let response = try decoder.decode(ToolCallsResponse.self, from: data)
        return response.toolCalls
    }

    // MARK: - Prompts

    func systemPrompt() async throws -> String {
        try await fetchPrompt(path: "/api/v1/prompts/system")
    }

    func identityPrompt() async throws -> String {
        try await fetchPrompt(path: "/api/v1/prompts/identity")
    }

    func soulPrompt() async throws -> String {
        try await fetchPrompt(path: "/api/v1/prompts/soul")
    }

    // MARK: - Trajectory

    func trajectoryEvents() async throws -> [TrajectoryEvent] {
        let url = baseURL.appendingPathComponent("/api/v1/trajectory")
        let (data, _) = try await session.data(from: url)
        return try decoder.decode([TrajectoryEvent].self, from: data)
    }

    // MARK: - Health

    func health() async throws -> Bool {
        let url = baseURL.appendingPathComponent("/api/v1/health")
        let (data, _) = try await session.data(from: url)
        let response = try decoder.decode(HealthResponse.self, from: data)
        return response.status == "ok"
    }

    // MARK: - Private

    private func fetchPrompt(path: String) async throws -> String {
        let url = baseURL.appendingPathComponent(path)
        let (data, _) = try await session.data(from: url)
        let response = try decoder.decode(PromptResponse.self, from: data)
        return response.content
    }
}

// MARK: - Response types

private struct EntriesResponse: Codable {
    let entries: [Entry]
}

private struct ToolCallsResponse: Codable {
    let toolCalls: [ToolCallPair]

    enum CodingKeys: String, CodingKey {
        case toolCalls = "tool_calls"
    }
}

struct ToolCallPair: Codable, Identifiable {
    let entryId: Int
    let call: ToolCall
    let result: ToolResultInfo?

    var id: String { call.id }

    enum CodingKeys: String, CodingKey {
        case entryId = "entry_id"
        case call
        case result
    }
}

struct ToolResultInfo: Codable {
    let entryId: Int
    let toolCallId: String
    let content: String

    enum CodingKeys: String, CodingKey {
        case entryId = "entry_id"
        case toolCallId = "tool_call_id"
        case content
    }
}

private struct PromptResponse: Codable {
    let content: String
}

private struct HealthResponse: Codable {
    let status: String
}
