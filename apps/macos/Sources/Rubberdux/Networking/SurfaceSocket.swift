import Combine
import Foundation
import Network

// MARK: - SurfaceDrive

/// The host-to-app command that drives the macOS UI by applying a batch of
/// `SurfaceOp`s. Mirrors the payload of `HostToAgent::SurfaceDrive` in
/// `src/protocol.rs`. `cmd`/`key` are forwarded so the app (in M-apply) can
/// stamp the resulting native UI-change echo with `Cause.command(cmd:key:)` for
/// echo dedup. See docs/agent/world/ecs-runtime.md (Theme 2a).
struct SurfaceDrive: Codable, Equatable {
    let ops: [SurfaceOp]
    let cmd: CmdId
    let key: IdempotencyKey

    init(ops: [SurfaceOp], cmd: CmdId, key: IdempotencyKey) {
        self.ops = ops
        self.cmd = cmd
        self.key = key
    }
}

// MARK: - Inbound frame (host -> app)

/// An inbound host-to-app frame, decoded from a length-prefixed JSON message.
/// Mirrors the serde externally-tagged `HostToAgent` enum in `src/protocol.rs`:
/// a struct variant `V` encodes as `{ "V": { ...fields... } }`. Only the
/// surface-driving variant is modelled here; any other variant decodes to
/// `.unsupported` so the socket can ignore non-surface frames without failing.
enum HostToAgentFrame: Codable, Equatable {
    case surfaceDrive(SurfaceDrive)
    case unsupported(tag: String)

    /// A coding key whose string value is a runtime-chosen serde variant name.
    private struct VariantKey: CodingKey {
        let stringValue: String
        var intValue: Int? { nil }
        init(_ stringValue: String) { self.stringValue = stringValue }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { return nil }
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: VariantKey.self)
        guard let key = container.allKeys.first else {
            throw DecodingError.dataCorrupted(
                DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "empty HostToAgent frame"
                )
            )
        }
        switch key.stringValue {
        case "SurfaceDrive":
            self = .surfaceDrive(try container.decode(SurfaceDrive.self, forKey: key))
        default:
            self = .unsupported(tag: key.stringValue)
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: VariantKey.self)
        switch self {
        case .surfaceDrive(let drive):
            try container.encode(drive, forKey: VariantKey("SurfaceDrive"))
        case .unsupported(let tag):
            throw EncodingError.invalidValue(
                tag,
                EncodingError.Context(
                    codingPath: encoder.codingPath,
                    debugDescription: "cannot encode an unsupported HostToAgent frame: \(tag)"
                )
            )
        }
    }
}

// MARK: - Outbound frame (app -> host)

/// An outbound app-to-host frame, encoded as a length-prefixed JSON message.
/// Mirrors the serde externally-tagged `AgentToHost` enum in `src/protocol.rs`
/// for the frames this client produces — the registration handshake plus the two
/// surface frames: a struct variant `V` encodes as `{ "V": { ...fields... } }`.
/// See docs/agent/world/ecs-runtime.md.
enum AgentToHostFrame: Codable, Equatable {
    /// The registration handshake (M-register): the FIRST frame a surface client
    /// sends after connecting, naming the App this socket observes/drives so the
    /// host's `SurfaceRouter` relays by App identity rather than accept order.
    /// Mirrors `AgentToHost::Hello { app_id }` — `{ "Hello": { "app_id": … } }`.
    case hello(appId: String)
    /// An observed AX surface snapshot (M-observe).
    case surfaceObservation(SurfaceObserved)
    /// A human-driven UI mutation echo (M-observe). Only `cause = .human` is
    /// sent; `command`/`peer` echoes are deduped before reaching this frame.
    case surfaceMutated(op: SurfaceOp, cause: Cause)

    private struct VariantKey: CodingKey {
        let stringValue: String
        var intValue: Int? { nil }
        init(_ stringValue: String) { self.stringValue = stringValue }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { return nil }
    }

    /// The `{ "app_id": ... }` payload of `AgentToHost::Hello`. The `app_id` key
    /// matches the serde field name on the Rust struct variant.
    private struct HelloPayload: Codable, Equatable {
        let appId: String

        private enum CodingKeys: String, CodingKey {
            case appId = "app_id"
        }
    }

    /// The `{ "observed": ... }` payload of `AgentToHost::SurfaceObservation`.
    private struct ObservationPayload: Codable, Equatable {
        let observed: SurfaceObserved
    }

    /// The `{ "op": ..., "cause": ... }` payload of `AgentToHost::SurfaceMutated`.
    private struct MutationPayload: Codable, Equatable {
        let op: SurfaceOp
        let cause: Cause
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: VariantKey.self)
        guard let key = container.allKeys.first else {
            throw DecodingError.dataCorrupted(
                DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "empty AgentToHost frame"
                )
            )
        }
        switch key.stringValue {
        case "Hello":
            let payload = try container.decode(HelloPayload.self, forKey: key)
            self = .hello(appId: payload.appId)
        case "SurfaceObservation":
            let payload = try container.decode(ObservationPayload.self, forKey: key)
            self = .surfaceObservation(payload.observed)
        case "SurfaceMutated":
            let payload = try container.decode(MutationPayload.self, forKey: key)
            self = .surfaceMutated(op: payload.op, cause: payload.cause)
        default:
            throw DecodingError.dataCorruptedError(
                forKey: key,
                in: container,
                debugDescription: "Unknown AgentToHost surface frame: \(key.stringValue)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: VariantKey.self)
        switch self {
        case .hello(let appId):
            try container.encode(
                HelloPayload(appId: appId),
                forKey: VariantKey("Hello")
            )
        case .surfaceObservation(let observed):
            try container.encode(
                ObservationPayload(observed: observed),
                forKey: VariantKey("SurfaceObservation")
            )
        case .surfaceMutated(let op, let cause):
            try container.encode(
                MutationPayload(op: op, cause: cause),
                forKey: VariantKey("SurfaceMutated")
            )
        }
    }
}

// MARK: - SurfaceSocket

/// The macOS app's surface client over the worker socket. It frames messages
/// exactly like `src/protocol.rs` (`write_message`/`read_message`): a 4-byte
/// big-endian length prefix followed by the JSON payload. On connect it sends the
/// `AgentToHost::Hello` registration as its first frame (`register(appId:)`).
/// Inbound it decodes `HostToAgent::SurfaceDrive` frames and publishes them on
/// `driveSubject` for M-apply to act on; outbound it encodes and sends
/// `AgentToHost::SurfaceObservation`/`SurfaceMutated` frames for M-observe.
///
/// This pass owns the wire (framing + serde-parity codecs). Translating a
/// `SurfaceDrive` into AX actions (M-apply) and producing observations from the
/// AX tree (M-observe) are later passes; the hooks they consume/call are
/// `driveSubject`, `send(observation:)`, and `send(mutation:cause:)`.
/// See docs/agent/world/ecs-runtime.md.
final class SurfaceSocket {

    /// The maximum frame size accepted, mirroring the 16 MiB cap in
    /// `read_message` (`src/protocol.rs`).
    static let maxFrameBytes = 16 * 1024 * 1024

    /// Inbound surface-drive commands from the host (consumed by M-apply).
    let driveSubject = PassthroughSubject<SurfaceDrive, Never>()

    private let connection: NWConnection
    private let queue: DispatchQueue
    private let encoder = JSONEncoder()
    private let decoder = JSONDecoder()
    private var inboundBuffer = Data()

    /// Wrap an already-created connection (e.g. the worker socket M-apply owns).
    init(connection: NWConnection, queue: DispatchQueue = DispatchQueue(label: "com.rubberdux.surface-socket")) {
        self.connection = connection
        self.queue = queue
    }

    /// Open a fresh TCP connection to the host's worker socket.
    convenience init(host: NWEndpoint.Host, port: NWEndpoint.Port) {
        self.init(connection: NWConnection(host: host, port: port, using: .tcp))
    }

    // MARK: Connection

    func connect() {
        connection.start(queue: queue)
        receiveLoop()
    }

    func disconnect() {
        connection.cancel()
    }

    // MARK: Registration handshake

    /// Send the registration `Hello` as the surface client's FIRST frame, naming
    /// the App this socket observes/drives so the host's `SurfaceRouter` relays by
    /// App identity. Must be sent immediately after `connect()` and before any
    /// observation; mirrors the `AgentToHost::Hello { app_id }` handshake the
    /// host's surface-client listener requires (`handle_surface_client` in
    /// `src/host.rs`). See docs/agent/world/ecs-runtime.md.
    func register(appId: String) {
        sendFrame(.hello(appId: appId))
    }

    // MARK: Sending (M-observe hooks)

    /// Send an observed AX surface snapshot to the host.
    func send(observation: SurfaceObserved) {
        sendFrame(.surfaceObservation(observation))
    }

    /// Send a human-driven UI mutation echo to the host. Callers must only pass
    /// `cause = .human`; `command`/`peer` echoes are deduped beforehand.
    func send(mutation op: SurfaceOp, cause: Cause) {
        sendFrame(.surfaceMutated(op: op, cause: cause))
    }

    private func sendFrame(_ frame: AgentToHostFrame) {
        guard let payload = try? encoder.encode(frame) else { return }
        let data = SurfaceSocket.frame(payload)
        connection.send(content: data, completion: .contentProcessed { _ in })
    }

    // MARK: Framing (mirrors src/protocol.rs)

    /// Prefix one JSON payload with its 4-byte big-endian length. Mirrors
    /// `write_message` in `src/protocol.rs`.
    static func frame(_ payload: Data) -> Data {
        var out = Data(capacity: 4 + payload.count)
        let length = UInt32(payload.count).bigEndian
        withUnsafeBytes(of: length) { out.append(contentsOf: $0) }
        out.append(payload)
        return out
    }

    // MARK: Receiving (M-apply source)

    private func receiveLoop() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data, !data.isEmpty {
                self.inboundBuffer.append(data)
                self.drainFrames()
            }
            if isComplete || error != nil {
                return
            }
            self.receiveLoop()
        }
    }

    /// Extract every complete length-prefixed frame from `inboundBuffer`,
    /// decode the surface-driving ones, and publish them. Mirrors the read side
    /// of `read_message` in `src/protocol.rs`.
    private func drainFrames() {
        while inboundBuffer.count >= 4 {
            let start = inboundBuffer.startIndex
            let length =
                (UInt32(inboundBuffer[start]) << 24) |
                (UInt32(inboundBuffer[start + 1]) << 16) |
                (UInt32(inboundBuffer[start + 2]) << 8) |
                UInt32(inboundBuffer[start + 3])

            guard length <= SurfaceSocket.maxFrameBytes else {
                disconnect()
                inboundBuffer.removeAll()
                return
            }

            let total = 4 + Int(length)
            guard inboundBuffer.count >= total else { return }

            let payload = inboundBuffer.subdata(in: (start + 4)..<(start + total))
            inboundBuffer.removeSubrange(start..<(start + total))

            if case .surfaceDrive(let drive)? = try? decoder.decode(HostToAgentFrame.self, from: payload) {
                driveSubject.send(drive)
            }
        }
    }
}
