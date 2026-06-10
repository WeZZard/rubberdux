import Combine
import Foundation

// MARK: - BoardSocket

/// A WebSocket subscription to the board-level event stream
/// (`/api/v1/ws/board`). Emits `BoardEvent` values whenever the backend
/// broadcasts a lifecycle change (app created, moved, archived, status changed).
///
/// Callers obtain a `BoardSocket` from `AppSocketRegistry`; the registry
/// ref-counts open/close so the underlying task is shared when multiple
/// subscribers exist.
final class BoardSocket {
    let eventSubject = PassthroughSubject<BoardEvent, Never>()

    private var task: URLSessionWebSocketTask?
    private let session: URLSession
    private let baseURL: URL
    private let decoder: JSONDecoder

    init(baseURL: URL, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.session = session
        self.decoder = JSONDecoder()
    }

    func connect() {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/ws/board"),
            resolvingAgainstBaseURL: false
        )!
        components.scheme = "ws"
        guard let url = components.url else { return }

        let wsTask = session.webSocketTask(with: url)
        wsTask.resume()
        task = wsTask
        receive(task: wsTask)
    }

    func disconnect() {
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
    }

    // MARK: - Private

    private func receive(task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let text) = message,
                   let data = text.data(using: .utf8),
                   let event = try? self.decoder.decode(BoardEvent.self, from: data) {
                    self.eventSubject.send(event)
                }
                self.receive(task: task)
            case .failure:
                break
            }
        }
    }
}
