import Foundation

// MARK: - ApprovalFlavor

/// The flavor of an approval interaction: a permission grant or a plan sign-off.
/// Mirrors `ApprovalFlavor` in `src/agent/interaction.rs`.
enum ApprovalFlavor: String, Codable, Equatable {
    case permission
    case plan
}

// MARK: - ChoiceOption

/// One selectable option in a `question` or `choice` interaction.
/// Mirrors `ChoiceOption` in `src/agent/interaction.rs`.
struct ChoiceOption: Codable, Equatable {
    let label: String
    let description: String
}

// MARK: - PreviewArtifact

/// A generated artifact presented for acknowledgement in a `preview` interaction.
/// Mirrors `PreviewArtifact` in `src/agent/interaction.rs`.
struct PreviewArtifact: Codable, Equatable {
    let mimeType: String
    let content: String

    enum CodingKeys: String, CodingKey {
        case mimeType = "mime_type"
        case content
    }
}

// MARK: - AgentInteraction

/// The fixed set of interaction primitives an agent may raise.
/// Mirrors `AgentInteraction` in `src/agent/interaction.rs`; the `kind` field is
/// the serde tag discriminator.
enum AgentInteraction: Codable, Equatable {
    case approval(requestId: String, appId: String, flavor: ApprovalFlavor, prompt: String)
    case question(requestId: String, appId: String, text: String, options: [ChoiceOption])
    case choice(requestId: String, appId: String, prompt: String, options: [ChoiceOption])
    case preview(requestId: String, appId: String, prompt: String, artifact: PreviewArtifact)

    var requestId: String {
        switch self {
        case .approval(let id, _, _, _): return id
        case .question(let id, _, _, _): return id
        case .choice(let id, _, _, _): return id
        case .preview(let id, _, _, _): return id
        }
    }

    var appId: String {
        switch self {
        case .approval(_, let id, _, _): return id
        case .question(_, let id, _, _): return id
        case .choice(_, let id, _, _): return id
        case .preview(_, let id, _, _): return id
        }
    }

    // MARK: Codable

    private enum KindKey: String, CodingKey {
        case kind
    }

    private enum ApprovalKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case appId = "app_id"
        case flavor
        case prompt
    }

    private enum QuestionKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case appId = "app_id"
        case text
        case options
    }

    private enum ChoiceKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case appId = "app_id"
        case prompt
        case options
    }

    private enum PreviewKeys: String, CodingKey {
        case kind
        case requestId = "request_id"
        case appId = "app_id"
        case prompt
        case artifact
    }

    init(from decoder: Decoder) throws {
        let kindContainer = try decoder.container(keyedBy: KindKey.self)
        let kind = try kindContainer.decode(String.self, forKey: .kind)

        switch kind {
        case "approval":
            let c = try decoder.container(keyedBy: ApprovalKeys.self)
            self = .approval(
                requestId: try c.decode(String.self, forKey: .requestId),
                appId: try c.decode(String.self, forKey: .appId),
                flavor: try c.decode(ApprovalFlavor.self, forKey: .flavor),
                prompt: try c.decode(String.self, forKey: .prompt)
            )

        case "question":
            let c = try decoder.container(keyedBy: QuestionKeys.self)
            self = .question(
                requestId: try c.decode(String.self, forKey: .requestId),
                appId: try c.decode(String.self, forKey: .appId),
                text: try c.decode(String.self, forKey: .text),
                options: try c.decode([ChoiceOption].self, forKey: .options)
            )

        case "choice":
            let c = try decoder.container(keyedBy: ChoiceKeys.self)
            self = .choice(
                requestId: try c.decode(String.self, forKey: .requestId),
                appId: try c.decode(String.self, forKey: .appId),
                prompt: try c.decode(String.self, forKey: .prompt),
                options: try c.decode([ChoiceOption].self, forKey: .options)
            )

        case "preview":
            let c = try decoder.container(keyedBy: PreviewKeys.self)
            self = .preview(
                requestId: try c.decode(String.self, forKey: .requestId),
                appId: try c.decode(String.self, forKey: .appId),
                prompt: try c.decode(String.self, forKey: .prompt),
                artifact: try c.decode(PreviewArtifact.self, forKey: .artifact)
            )

        default:
            throw DecodingError.dataCorruptedError(
                forKey: KindKey.kind,
                in: kindContainer,
                debugDescription: "Unknown AgentInteraction kind: \(kind)"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .approval(let requestId, let appId, let flavor, let prompt):
            var c = encoder.container(keyedBy: ApprovalKeys.self)
            try c.encode("approval", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encode(appId, forKey: .appId)
            try c.encode(flavor, forKey: .flavor)
            try c.encode(prompt, forKey: .prompt)

        case .question(let requestId, let appId, let text, let options):
            var c = encoder.container(keyedBy: QuestionKeys.self)
            try c.encode("question", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encode(appId, forKey: .appId)
            try c.encode(text, forKey: .text)
            try c.encode(options, forKey: .options)

        case .choice(let requestId, let appId, let prompt, let options):
            var c = encoder.container(keyedBy: ChoiceKeys.self)
            try c.encode("choice", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encode(appId, forKey: .appId)
            try c.encode(prompt, forKey: .prompt)
            try c.encode(options, forKey: .options)

        case .preview(let requestId, let appId, let prompt, let artifact):
            var c = encoder.container(keyedBy: PreviewKeys.self)
            try c.encode("preview", forKey: .kind)
            try c.encode(requestId, forKey: .requestId)
            try c.encode(appId, forKey: .appId)
            try c.encode(prompt, forKey: .prompt)
            try c.encode(artifact, forKey: .artifact)
        }
    }
}
