import Foundation

// MARK: - InteractionResponse

/// The fixed set of replies to an `AgentInteraction`.
/// Mirrors `InteractionResponse` in `src/agent/interaction.rs`; the `kind` field
/// is the serde tag discriminator.
enum InteractionResponse: Codable, Equatable {
    /// An approval interaction was granted.
    case approved(requestId: String, flavor: ApprovalFlavor)
    /// An approval interaction was declined with a reason.
    case declined(requestId: String, flavor: ApprovalFlavor, reason: String)
    /// A question or choice interaction was answered.
    case answered(requestId: String, selected: Int?, reply: String?)
    /// A preview interaction was acknowledged.
    case acknowledged(requestId: String)

    var requestId: String {
        switch self {
        case .approved(let id, _): return id
        case .declined(let id, _, _): return id
        case .answered(let id, _, _): return id
        case .acknowledged(let id): return id
        }
    }

    // MARK: Codable

    private enum KindKey: String, CodingKey {
        case kind
    }

    private enum ApprovedKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case flavor
    }

    private enum DeclinedKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case flavor
        case reason
    }

    private enum AnsweredKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case selected
        case reply
    }

    private enum AcknowledgedKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
    }

    init(from decoder: Decoder) throws {
        let kindContainer = try decoder.container(keyedBy: KindKey.self)
        let kind = try kindContainer.decode(String.self, forKey: .kind)

        switch kind {
        case "approved":
            let c = try decoder.container(keyedBy: ApprovedKeys.self)
            self = .approved(
                requestId: try c.decode(String.self, forKey: .requestId),
                flavor: try c.decode(ApprovalFlavor.self, forKey: .flavor)
            )

        case "declined":
            let c = try decoder.container(keyedBy: DeclinedKeys.self)
            self = .declined(
                requestId: try c.decode(String.self, forKey: .requestId),
                flavor: try c.decode(ApprovalFlavor.self, forKey: .flavor),
                reason: try c.decode(String.self, forKey: .reason)
            )

        case "answered":
            let c = try decoder.container(keyedBy: AnsweredKeys.self)
            self = .answered(
                requestId: try c.decode(String.self, forKey: .requestId),
                selected: try c.decodeIfPresent(Int.self, forKey: .selected),
                reply: try c.decodeIfPresent(String.self, forKey: .reply)
            )

        case "acknowledged":
            let c = try decoder.container(keyedBy: AcknowledgedKeys.self)
            self = .acknowledged(
                requestId: try c.decode(String.self, forKey: .requestId)
            )

        default:
            throw DecodingError.dataCorruptedError(
                forKey: KindKey.kind,
                in: kindContainer,
                debugDescription: "Unknown InteractionResponse kind: \(kind)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .approved(let requestId, let flavor):
            var c = encoder.container(keyedBy: ApprovedKeys.self)
            try c.encode("approved", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encode(flavor, forKey: .flavor)

        case .declined(let requestId, let flavor, let reason):
            var c = encoder.container(keyedBy: DeclinedKeys.self)
            try c.encode("declined", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encode(flavor, forKey: .flavor)
            try c.encode(reason, forKey: .reason)

        case .answered(let requestId, let selected, let reply):
            var c = encoder.container(keyedBy: AnsweredKeys.self)
            try c.encode("answered", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encodeIfPresent(selected, forKey: .selected)
            try c.encodeIfPresent(reply, forKey: .reply)

        case .acknowledged(let requestId):
            var c = encoder.container(keyedBy: AcknowledgedKeys.self)
            try c.encode("acknowledged", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
        }
    }
}
