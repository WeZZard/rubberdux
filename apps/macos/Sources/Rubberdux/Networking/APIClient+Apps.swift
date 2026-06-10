import Foundation

// MARK: - Request / response types

private struct AppsListResponse: Codable {
    let apps: [App]
}

private struct AppEntriesResponse: Codable {
    let entries: [Entry]
}

private struct AppTrajectoryResponse: Codable {
    let events: [TrajectoryEvent]
}

private struct InteractionsListResponse: Codable {
    let interactions: [AgentInteraction]
}

// MARK: - APIClient apps extension

extension APIClient {

    // MARK: - Generic JSON-body helpers

    /// POST a JSON-encodable body to `path` and decode the response as `T`.
    func post<Body: Encodable, Response: Decodable>(
        path: String,
        body: Body
    ) async throws -> Response {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, _) = try await session.data(for: request)
        return try decoder.decode(Response.self, from: data)
    }

    /// POST a JSON-encodable body to `path`; discard the response body (for `202`/`204`).
    func postVoid<Body: Encodable>(path: String, body: Body) async throws {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        _ = try await session.data(for: request)
    }

    /// PATCH a JSON-encodable body to `path` and decode the response as `T`.
    func patch<Body: Encodable, Response: Decodable>(
        path: String,
        body: Body
    ) async throws -> Response {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "PATCH"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, _) = try await session.data(for: request)
        return try decoder.decode(Response.self, from: data)
    }

    /// DELETE `path`; discard the response body (for `204`).
    func deleteVoid(path: String) async throws {
        let url = baseURL.appendingPathComponent(path)
        var request = URLRequest(url: url)
        request.httpMethod = "DELETE"
        _ = try await session.data(for: request)
    }

    // MARK: - Apps

    /// List all Apps on the board. Mirrors `GET /api/v1/apps`.
    func apps() async throws -> [App] {
        let url = baseURL.appendingPathComponent("/api/v1/apps")
        let (data, _) = try await session.data(from: url)
        return try decoder.decode(AppsListResponse.self, from: data).apps
    }

    /// Create an App from a task and return the new App immediately.
    /// Mirrors `POST /api/v1/apps`.
    func createApp(task: String, position: BoardPosition) async throws -> App {
        struct Body: Encodable {
            let task: String
            let position: BoardPosition
        }
        return try await post(
            path: "/api/v1/apps",
            body: Body(task: task, position: position)
        )
    }

    /// Partially update an App (position, title, user_locked).
    /// Mirrors `PATCH /api/v1/apps/{id}`.
    func patchApp(
        id: String,
        position: BoardPosition? = nil,
        title: String? = nil,
        userLocked: Bool? = nil
    ) async throws -> App {
        struct Body: Encodable {
            let position: BoardPosition?
            let title: String?
            let userLocked: Bool?

            enum CodingKeys: String, CodingKey {
                case position
                case title
                case userLocked = "user_locked"
            }
        }
        return try await patch(
            path: "/api/v1/apps/\(id)",
            body: Body(position: position, title: title, userLocked: userLocked)
        )
    }

    /// Move an App to a new board position. Convenience wrapper over `patchApp`.
    func moveApp(id: String, position: BoardPosition) async throws -> App {
        try await patchApp(id: id, position: position)
    }

    /// Archive an App. Mirrors `DELETE /api/v1/apps/{id}`.
    func archiveApp(id: String) async throws {
        try await deleteVoid(path: "/api/v1/apps/\(id)")
    }

    // MARK: - Per-app entries / trajectory

    /// Snapshot the App's history entries. Mirrors `GET /api/v1/apps/{id}/entries`.
    func entries(appID: String) async throws -> [Entry] {
        let url = baseURL.appendingPathComponent("/api/v1/apps/\(appID)/entries")
        let (data, _) = try await session.data(from: url)
        return try decoder.decode(AppEntriesResponse.self, from: data).entries
    }

    /// Snapshot the App's trajectory events. Mirrors `GET /api/v1/apps/{id}/trajectory`.
    func trajectory(appID: String) async throws -> [TrajectoryEvent] {
        let url = baseURL.appendingPathComponent("/api/v1/apps/\(appID)/trajectory")
        let (data, _) = try await session.data(from: url)
        return try decoder.decode(AppTrajectoryResponse.self, from: data).events
    }

    // MARK: - Tasks

    /// Deliver a free-form user message to the App's running worker.
    /// Mirrors `POST /api/v1/apps/{id}/tasks` (body `{ "text": ... }`).
    func sendTask(appID: String, text: String) async throws {
        struct Body: Encodable {
            let text: String
        }
        try await postVoid(
            path: "/api/v1/apps/\(appID)/tasks",
            body: Body(text: text)
        )
    }

    // MARK: - Interactions

    /// List pending interactions for an App.
    /// Mirrors `GET /api/v1/apps/{id}/interactions`.
    func interactions(appID: String) async throws -> [AgentInteraction] {
        let url = baseURL.appendingPathComponent("/api/v1/apps/\(appID)/interactions")
        let (data, _) = try await session.data(from: url)
        return try decoder.decode(InteractionsListResponse.self, from: data).interactions
    }

    /// Answer a pending interaction.
    /// Mirrors `POST /api/v1/apps/{id}/interactions/{request_id}`.
    func respondToInteraction(
        appID: String,
        requestID: String,
        response: InteractionResponse
    ) async throws {
        try await postVoid(
            path: "/api/v1/apps/\(appID)/interactions/\(requestID)",
            body: response
        )
    }
}
