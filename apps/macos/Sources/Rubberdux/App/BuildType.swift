import Foundation

enum BuildType {
    case debug
    case release
    case distributed

    static var current: BuildType {
        guard let path = Bundle.main.executablePath else { return .release }
        if path.contains("/target/debug/") { return .debug }
        if path.contains("/target/release/") { return .release }
        if Bundle.main.bundlePath.hasSuffix(".app") { return .distributed }
        return .release
    }

    var supportsLaunchAtLogin: Bool {
        self == .distributed
    }
}
