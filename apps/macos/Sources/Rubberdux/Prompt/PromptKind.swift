enum PromptKind {
    case system
    case identity
    case soul

    var title: String {
        switch self {
        case .system: return "System Prompt"
        case .identity: return "Identity Prompt"
        case .soul: return "Soul Prompt"
        }
    }
}
