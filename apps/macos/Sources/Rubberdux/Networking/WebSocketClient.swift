import Combine
import Foundation

final class WebSocketClient {
    enum ConnectionState {
        case disconnected
        case connecting
        case connected
    }

    @Published private(set) var connectionState: ConnectionState = .disconnected

    let entrySubject = PassthroughSubject<EntryNotification, Never>()
    let trajectorySubject = PassthroughSubject<TrajectoryEvent, Never>()

    private var entryTask: URLSessionWebSocketTask?
    private var trajectoryTask: URLSessionWebSocketTask?
    private let session: URLSession
    private let baseURL: URL
    private let decoder: JSONDecoder

    init(baseURL: URL = URL(string: "http://localhost:19385")!) {
        self.baseURL = baseURL
        self.session = URLSession.shared
        self.decoder = JSONDecoder()
    }

    func connect() {
        connectionState = .connecting
        connectEntries()
        connectTrajectory()
        connectionState = .connected
    }

    func disconnect() {
        entryTask?.cancel(with: .goingAway, reason: nil)
        trajectoryTask?.cancel(with: .goingAway, reason: nil)
        entryTask = nil
        trajectoryTask = nil
        connectionState = .disconnected
    }

    // MARK: - Private

    private func connectEntries() {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/ws/entries"),
            resolvingAgainstBaseURL: false
        )!
        components.scheme = "ws"
        guard let url = components.url else { return }

        let task = session.webSocketTask(with: url)
        task.resume()
        entryTask = task
        receiveEntries(task: task)
    }

    private func connectTrajectory() {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/ws/trajectory"),
            resolvingAgainstBaseURL: false
        )!
        components.scheme = "ws"
        guard let url = components.url else { return }

        let task = session.webSocketTask(with: url)
        task.resume()
        trajectoryTask = task
        receiveTrajectory(task: task)
    }

    private func receiveEntries(task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let text) = message, let data = text.data(using: .utf8) {
                    if let wrapper = try? self.decoder.decode(EntryWsWrapper.self, from: data) {
                        self.entrySubject.send(wrapper.toNotification())
                    }
                }
                self.receiveEntries(task: task)
            case .failure:
                self.connectionState = .disconnected
            }
        }
    }

    private func receiveTrajectory(task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let text) = message, let data = text.data(using: .utf8) {
                    if let wrapper = try? self.decoder.decode(TrajectoryWsWrapper.self, from: data) {
                        self.trajectorySubject.send(wrapper.event)
                    }
                }
                self.receiveTrajectory(task: task)
            case .failure:
                self.connectionState = .disconnected
            }
        }
    }
}

// MARK: - WebSocket message wrappers

struct EntryNotification {
    let entry: Entry
    let isFinal: Bool
}

private struct EntryWsWrapper: Codable {
    let type: String
    let entry: Entry
    let isFinal: Bool

    enum CodingKeys: String, CodingKey {
        case type
        case entry
        case isFinal = "is_final"
    }

    func toNotification() -> EntryNotification {
        EntryNotification(entry: entry, isFinal: isFinal)
    }
}

private struct TrajectoryWsWrapper: Codable {
    let type: String
    let event: TrajectoryEvent
}
