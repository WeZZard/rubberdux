import Foundation

// MARK: - Surface primitive identifiers and versioning

/// A surface within the macOS app. Mirrors `SurfaceId` (`u32`) in
/// `src/agent/world/surface.rs`.
typealias SurfaceId = UInt32

/// An element within a surface (AX node or logical UI element). Mirrors
/// `ElementId` (`u32`) in `src/agent/world/surface.rs`.
typealias ElementId = UInt32

/// Per-surface optimistic-concurrency monotone counter. Mirrors
/// `SurfaceVersion` (`u64`) in `src/agent/world/surface.rs`.
typealias SurfaceVersion = UInt64

/// A dispatched-command identifier. Mirrors `CmdId` (`u32`) in
/// `src/agent/world/world.rs`; carried by `Cause::Command` so a native UI-change
/// echo can be correlated to the originating command for echo dedup.
typealias CmdId = UInt32

// MARK: - SurfacePoint

/// A `(column, row)` pixel point within a surface, used as an optional hit-test
/// override on `SurfaceOp.click`. Mirrors `Point` (`(u32, u32)`) in
/// `src/agent/world/surface.rs`, which serde encodes as a two-element JSON
/// array `[column, row]`.
struct SurfacePoint: Codable, Equatable {
    let column: UInt32
    let row: UInt32

    init(column: UInt32, row: UInt32) {
        self.column = column
        self.row = row
    }

    init(from decoder: Decoder) throws {
        var container = try decoder.unkeyedContainer()
        column = try container.decode(UInt32.self)
        row = try container.decode(UInt32.self)
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.unkeyedContainer()
        try container.encode(column)
        try container.encode(row)
    }
}

// MARK: - String newtypes

/// A surface identifier whose wire representation is a transparent string, used
/// to mirror the Rust newtype structs in `src/agent/world/surface.rs` (e.g.
/// `Hash(pub String)`). serde encodes a newtype struct transparently as its
/// inner value, so each of these encodes/decodes as a bare JSON string.
protocol SurfaceStringIdentifier: Codable, Equatable {
    var rawValue: String { get }
    init(_ rawValue: String)
}

extension SurfaceStringIdentifier {
    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        self.init(try container.decode(String.self))
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// A content digest of the AX tree at one observation tick. Mirrors `Hash` in
/// `src/agent/world/surface.rs`.
struct Hash: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// A navigation route within a surface. Mirrors `Route` in
/// `src/agent/world/surface.rs`.
struct Route: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// A component tree to render into a surface region. Mirrors `ComponentSpec` in
/// `src/agent/world/surface.rs`.
struct ComponentSpec: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// An AX/UI selection state. Mirrors `Selection` in
/// `src/agent/world/surface.rs`.
struct Selection: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// A viewport descriptor (visible rect within a scrollable region). Mirrors
/// `Viewport` in `src/agent/world/surface.rs`.
struct Viewport: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// A window state descriptor. Mirrors `WindowState` in
/// `src/agent/world/surface.rs`.
struct WindowState: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// The WAL-milestone idempotency key stamped on a `Cause::Command` echo. Mirrors
/// `IdempotencyKey` in `src/agent/world/surface.rs`.
struct IdempotencyKey: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

/// A durable peer-message envelope id. Mirrors `PeerEnvelopeId` in
/// `src/agent/world/surface.rs`.
struct PeerEnvelopeId: SurfaceStringIdentifier {
    let rawValue: String
    init(_ rawValue: String) { self.rawValue = rawValue }
}

// MARK: - Cause

/// The cause the OS attaches to a native UI change signal — the basis of echo
/// dedup. Mirrors `Cause` in `src/agent/world/surface.rs`, which serde encodes
/// internally tagged on the `cause` field with `snake_case` variant names
/// (`human` / `command` / `peer`). See docs/agent/world/ecs-runtime.md (Cause).
enum Cause: Codable, Equatable {
    /// The change originated from direct human interaction with the UI.
    case human
    /// The change is the screen's echo of an agent-dispatched UI command,
    /// identified by the originating command and idempotency key.
    case command(cmd: CmdId, key: IdempotencyKey)
    /// The change is the screen's echo of a cross-World drive, identified by the
    /// sender's durable envelope.
    case peer(envelope: PeerEnvelopeId)

    private enum TagKey: String, CodingKey {
        case cause
    }

    private enum CommandKeys: String, CodingKey {
        case cause
        case cmd
        case key
    }

    private enum PeerKeys: String, CodingKey {
        case cause
        case envelope
    }

    init(from decoder: Decoder) throws {
        let tagContainer = try decoder.container(keyedBy: TagKey.self)
        let tag = try tagContainer.decode(String.self, forKey: .cause)
        switch tag {
        case "human":
            self = .human
        case "command":
            let c = try decoder.container(keyedBy: CommandKeys.self)
            self = .command(
                cmd: try c.decode(CmdId.self, forKey: .cmd),
                key: try c.decode(IdempotencyKey.self, forKey: .key)
            )
        case "peer":
            let c = try decoder.container(keyedBy: PeerKeys.self)
            self = .peer(envelope: try c.decode(PeerEnvelopeId.self, forKey: .envelope))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: TagKey.cause,
                in: tagContainer,
                debugDescription: "Unknown Cause tag: \(tag)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .human:
            var c = encoder.container(keyedBy: TagKey.self)
            try c.encode("human", forKey: .cause)
        case .command(let cmd, let key):
            var c = encoder.container(keyedBy: CommandKeys.self)
            try c.encode("command", forKey: .cause)
            try c.encode(cmd, forKey: .cmd)
            try c.encode(key, forKey: .key)
        case .peer(let envelope):
            var c = encoder.container(keyedBy: PeerKeys.self)
            try c.encode("peer", forKey: .cause)
            try c.encode(envelope, forKey: .envelope)
        }
    }
}

// MARK: - SurfaceObserved

/// An observed AX surface snapshot reported from the macOS app to the host.
/// Mirrors the `SurfaceObserved` struct in `src/protocol.rs` field-for-field
/// (serde default field names; the optional fields encode as explicit `null`).
/// See docs/agent/world/ecs-runtime.md (Theme 2b).
struct SurfaceObserved: Codable, Equatable {
    let surface: SurfaceId
    let version: SurfaceVersion
    let axDigest: Hash
    let focus: ElementId?
    let selection: Selection?
    let viewport: Viewport
    let window: WindowState
    let cursor: SurfacePoint?

    init(
        surface: SurfaceId,
        version: SurfaceVersion,
        axDigest: Hash,
        focus: ElementId?,
        selection: Selection?,
        viewport: Viewport,
        window: WindowState,
        cursor: SurfacePoint?
    ) {
        self.surface = surface
        self.version = version
        self.axDigest = axDigest
        self.focus = focus
        self.selection = selection
        self.viewport = viewport
        self.window = window
        self.cursor = cursor
    }

    private enum CodingKeys: String, CodingKey {
        case surface
        case version
        case axDigest = "ax_digest"
        case focus
        case selection
        case viewport
        case window
        case cursor
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        surface = try c.decode(SurfaceId.self, forKey: .surface)
        version = try c.decode(SurfaceVersion.self, forKey: .version)
        axDigest = try c.decode(Hash.self, forKey: .axDigest)
        focus = try c.decodeIfPresent(ElementId.self, forKey: .focus)
        selection = try c.decodeIfPresent(Selection.self, forKey: .selection)
        viewport = try c.decode(Viewport.self, forKey: .viewport)
        window = try c.decode(WindowState.self, forKey: .window)
        cursor = try c.decodeIfPresent(SurfacePoint.self, forKey: .cursor)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(surface, forKey: .surface)
        try c.encode(version, forKey: .version)
        try c.encode(axDigest, forKey: .axDigest)
        // serde emits Option fields as explicit `null` (no skip_serializing_if),
        // so encode the optionals (not encodeIfPresent) to keep the wire shape.
        try c.encode(focus, forKey: .focus)
        try c.encode(selection, forKey: .selection)
        try c.encode(viewport, forKey: .viewport)
        try c.encode(window, forKey: .window)
        try c.encode(cursor, forKey: .cursor)
    }
}
