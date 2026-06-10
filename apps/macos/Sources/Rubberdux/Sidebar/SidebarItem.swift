import Foundation

enum SidebarItem: Hashable, CustomStringConvertible {
    /// The dot-grid whiteboard; the primary content surface and default
    /// selection. The remaining items are the debug views, kept reachable.
    case whiteboard

    case conversation
    case systemPrompts
    case identityPrompt
    case soulPrompt

    case liveEvents
    case rawTrajectory

    var description: String {
        switch self {
        case .whiteboard: return "Whiteboard"
        case .conversation: return "Conversation"
        case .systemPrompts: return "System"
        case .identityPrompt: return "Identity"
        case .soulPrompt: return "Soul"
        case .liveEvents: return "Live Events"
        case .rawTrajectory: return "Raw Trajectory"
        }
    }

    var isHeader: Bool {
        switch self {
        case .systemPrompts: return true
        default: return false
        }
    }

    var children: [SidebarItem]? {
        switch self {
        case .systemPrompts: return [.identityPrompt, .soulPrompt]
        default: return nil
        }
    }

    static var topLevel: [SidebarItem] {
        [.whiteboard, .conversation, .systemPrompts, .liveEvents]
    }

}
