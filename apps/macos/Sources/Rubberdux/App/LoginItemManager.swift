import Foundation
import ServiceManagement

enum LoginItemManager {
    private static var workspaceLabel: String {
        switch BuildType.current {
        case .debug: return "com.wezzard.rubberdux.app.debug"
        case .release: return "com.wezzard.rubberdux.app.release"
        case .distributed: return "com.wezzard.rubberdux.app"
        }
    }

    private static var plistURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents")
            .appendingPathComponent("\(workspaceLabel).plist")
    }

    static func isEnabled() -> Bool {
        if BuildType.current.usesLaunchAgent {
            return FileManager.default.fileExists(atPath: plistURL.path)
        }
        return SMAppService.mainApp.status == .enabled
    }

    static func setEnabled(_ enabled: Bool) throws {
        if BuildType.current.usesLaunchAgent {
            if enabled {
                try registerLaunchAgent()
            } else {
                try unregisterLaunchAgent()
            }
        } else {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        }
    }

    private static func registerLaunchAgent() throws {
        guard let binaryPath = Bundle.main.executablePath else { return }

        let launchAgentsDir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents")
        try FileManager.default.createDirectory(at: launchAgentsDir, withIntermediateDirectories: true)

        let plist: [String: Any] = [
            "Label": workspaceLabel,
            "ProgramArguments": [binaryPath],
            "RunAtLoad": true,
            "LimitLoadToSessionType": "Aqua",
        ]

        let data = try PropertyListSerialization.data(
            fromPropertyList: plist,
            format: .xml,
            options: 0
        )
        try data.write(to: plistURL)
    }

    private static func unregisterLaunchAgent() throws {
        let uid = getuid()
        _ = shell("/bin/launchctl", "bootout", "gui/\(uid)/\(workspaceLabel)")
        if FileManager.default.fileExists(atPath: plistURL.path) {
            try FileManager.default.removeItem(at: plistURL)
        }
    }

    @discardableResult
    private static func shell(_ executable: String, _ arguments: String...) -> Int32 {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        do {
            try process.run()
            process.waitUntilExit()
            return process.terminationStatus
        } catch {
            return -1
        }
    }
}
