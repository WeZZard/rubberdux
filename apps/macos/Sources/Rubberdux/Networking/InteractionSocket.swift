import Combine
import Foundation

// MARK: - InteractionSocketEvent

/// A live interaction lifecycle event for one App, decoded from the per-App
/// interaction WebSocket. Mirrors `InteractionWsMessage` in
/// `src/gateway/apps_stream.rs` (`interaction_raised` / `resolved`).
enum InteractionSocketEvent: Equatable {
    /// An interaction was raised and awaits a human answer.
    case raised(AgentInteraction)
    /// A previously raised interaction was answered and cleared.
    case resolved(requestId: String)
}

// MARK: - InteractionSocket

/// A bidirectional WebSocket subscription to one App's interaction stream
/// (`/api/v1/ws/apps/{id}/interactions`). Outbound it decodes
/// `interaction_raised` / `resolved` frames and forwards them on a Combine
/// subject; inbound it sends a `respond` frame carrying an `InteractionResponse`.
/// Mirrors `InteractionWsMessage` / `InteractionInbound` in
/// `src/gateway/apps_stream.rs`.
///
/// See `docs/apps/macos/whiteboard-interaction.md` for the interaction-surface
/// design record.
final class InteractionSocket {

    let eventSubject = PassthroughSubject<InteractionSocketEvent, Never>()

    let appID: String

    private var task: URLSessionWebSocketTask?
    private let session: URLSession
    private let baseURL: URL
    private let decoder: JSONDecoder
    private let encoder: JSONEncoder

    init(appID: String, baseURL: URL, session: URLSession = .shared) {
        self.appID = appID
        self.baseURL = baseURL
        self.session = session
        self.decoder = JSONDecoder()
        self.encoder = JSONEncoder()
    }

    // MARK: - Connection

    func connect() {
        var components = URLComponents(
            url: baseURL.appendingPathComponent("/api/v1/ws/apps/\(appID)/interactions"),
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

    // MARK: - Sending

    /// Send a `respond` frame answering a raised interaction. The POST fallback
    /// lives on `APIClient.respondToInteraction`; this is the live path used while
    /// the socket is connected.
    func respond(_ response: InteractionResponse) {
        guard let task,
              let json = try? encoder.encode(RespondInbound(response: response)),
              let text = String(data: json, encoding: .utf8)
        else { return }
        task.send(.string(text)) { error in
            if let error {
                NSLog("[InteractionSocket] respond send failed: %@", String(describing: error))
            }
        }
    }

    // MARK: - Private

    private func receive(task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let message):
                if case .string(let text) = message,
                   let data = text.data(using: .utf8),
                   let frame = try? self.decoder.decode(InteractionWsFrame.self, from: data),
                   let event = frame.toEvent() {
                    self.eventSubject.send(event)
                }
                self.receive(task: task)
            case .failure:
                break
            }
        }
    }
}

// MARK: - Wire frames

/// Inbound `respond` frame, mirroring `InteractionInbound::Respond` in
/// `src/gateway/apps_stream.rs`. The `type` tag is the wire discriminator.
private struct RespondInbound: Encodable {
    let type = "respond"
    let response: InteractionResponse
}

/// Outbound interaction frame, mirroring `InteractionWsMessage`. The `type` tag
/// selects between `interaction_raised` (carrying the interaction) and
/// `resolved` (carrying the request id).
private struct InteractionWsFrame: Decodable {
    let type: String
    let interaction: AgentInteraction?
    let requestId: String?

    enum CodingKeys: String, CodingKey {
        case type
        case interaction
        case requestId = "request_id"
    }

    func toEvent() -> InteractionSocketEvent? {
        switch type {
        case "interaction_raised":
            return interaction.map(InteractionSocketEvent.raised)
        case "resolved":
            return requestId.map(InteractionSocketEvent.resolved)
        default:
            return nil
        }
    }
}
