import Foundation

enum BuildType {
    case debug
    case release
    case distribute

    static var current: BuildType {
        switch Bundle.main.bundleIdentifier {
        case "com.wezzard.rubberdux.debug": return .debug
        case "com.wezzard.rubberdux.release": return .release
        case "com.wezzard.rubberdux": return .distribute
        default: return .debug
        }
    }
}
