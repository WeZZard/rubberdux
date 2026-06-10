import Combine
import Foundation

// MARK: - AppSocket

/// A WebSocket subscription to a single App's live entry and trajectory streams.
/// Mirrors the per-app WebSocket paths `/api/v1/ws/apps/{id}/entries` and
/// `/api/v1/ws/apps/{id}/trajectory`.
///
/// Callers obtain an `AppSocket` from `AppSocketRegistry`; the registry
/// ref-counts open/close so that opening the same app ID twice reuses the
/// existing socket rather than opening a second connection.
final class AppSocket {
    let entrySubject = PassthroughSubject<EntryNotification, Never>()
    let trajectorySubject = PassthroughSubject<TrajectoryEvent, Never>()

    let appID: String

    private var entryTask: URLSessionWebSocketTask?
    private var trajectoryTask: URLSessionWebSocketTask?
    private let session: URLSession
    private let baseURL: URL
    private let decoder: JSONDecoder

    init(appID: String, baseURL: URL, session: URLSession = .shared) {
        self.appID = appID
        self.baseURL = baseURL
        self.session = session
        self.decoder = JSONDecoder()
    }

    func connect() {
        connectEntries()
        connectTrajectory()
    }

    func disconnect() {
        entryTask?.cancel(with: .goingAway, reason: nil)
        trajectoryTask?.cancel(with: .goingAway, reason: nil)
        entryTask = nil
        trajectoryTask = nil
    }

    // MARK: - Private

    private func connectEntries() {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/ws/apps/\(appID)/entries"),
            resolvingAgainstBaseURL: false
        )!
        components.scheme = "ws"
        guard let url = components.url else { return }

        let wsTask = session.webSocketTask(with: url)
        wsTask.resume()
        entryTask = wsTask
        receiveEntries(task: wsTask)
    }

    private func connectTrajectory() {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/ws/apps/\(appID)/trajectory"),
            resolvingAgainstBaseURL: false
        )!
        components.scheme = "ws"
        guard let url = components.url else { return }

        let wsTask = session.webSocketTask(with: url)
        wsTask.resume()
        trajectoryTask = wsTask
        receiveTrajectory(task: wsTask)
    }

    private func receiveEntries(task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let text) = message,
                   let data = text.data(using: .utf8),
                   let wrapper = try? self.decoder.decode(AppEntryWsWrapper.self, from: data) {
                    self.entrySubject.send(wrapper.toNotification())
                }
                self.receiveEntries(task: task)
            case .failure:
                break
            }
        }
    }

    private func receiveTrajectory(task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let text) = message,
                   let data = text.data(using: .utf8),
                   let wrapper = try? self.decoder.decode(AppTrajectoryWsWrapper.self, from: data) {
                    self.trajectorySubject.send(wrapper.event)
                }
                self.receiveTrajectory(task: task)
            case .failure:
                break
            }
        }
    }
}

// MARK: - WebSocket message wrappers

private struct AppEntryWsWrapper: Codable {
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

private struct AppTrajectoryWsWrapper: Codable {
    let type: String
    let event: TrajectoryEvent
}
