import AppKit

enum AppearanceMode: Int, CaseIterable {
    case dockAndMenuBar = 0
    case dockOnly = 1
    case menuBarOnly = 2

    static var current: AppearanceMode {
        AppearanceMode(rawValue: UserDefaults.standard.integer(forKey: "appearanceMode")) ?? .dockAndMenuBar
    }

    static func save(_ mode: AppearanceMode) {
        UserDefaults.standard.set(mode.rawValue, forKey: "appearanceMode")
    }

    var activationPolicy: NSApplication.ActivationPolicy {
        switch self {
        case .dockAndMenuBar, .dockOnly: return .regular
        case .menuBarOnly: return .accessory
        }
    }

    var showsStatusItem: Bool {
        switch self {
        case .dockAndMenuBar, .menuBarOnly: return true
        case .dockOnly: return false
        }
    }

    var description: String {
        switch self {
        case .dockAndMenuBar: return "Dock and menu bar"
        case .dockOnly: return "Dock only"
        case .menuBarOnly: return "Menu bar only"
        }
    }
}
