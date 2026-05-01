import Foundation

enum LaunchAgentManager {
    static let label = "com.wezzard.rubberdux"

    static var plistURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents")
            .appendingPathComponent("\(label).plist")
    }

    static func register(binaryPath: URL, env: [String: String] = [:]) throws {
        let logDir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".rubberdux/logs")
        try FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)

        let plist: [String: Any] = [
            "Label": label,
            "ProgramArguments": [binaryPath.path, "--host"],
            "WorkingDirectory": binaryPath
                .deletingLastPathComponent()
                .deletingLastPathComponent()
                .deletingLastPathComponent()
                .path,
            "RunAtLoad": true,
            "KeepAlive": ["SuccessfulExit": false],
            "StandardOutPath": logDir.appendingPathComponent("rubberdux.log").path,
            "StandardErrorPath": logDir.appendingPathComponent("rubberdux.error.log").path,
            "EnvironmentVariables": env,
        ]

        let data = try PropertyListSerialization.data(
            fromPropertyList: plist,
            format: .xml,
            options: 0
        )
        try data.write(to: plistURL)

        let uid = getuid()
        let result = shell("/bin/launchctl", "bootstrap", "gui/\(uid)", plistURL.path)
        if result != 0 {
            try? FileManager.default.removeItem(at: plistURL)
            throw BackendProvider.Error.launchFailed("launchctl bootstrap failed with exit code \(result)")
        }
    }

    static func unregister() throws {
        let uid = getuid()
        _ = shell("/bin/launchctl", "bootout", "gui/\(uid)/\(label)")
        if FileManager.default.fileExists(atPath: plistURL.path) {
            try FileManager.default.removeItem(at: plistURL)
        }
    }

    static func isRegistered() -> Bool {
        FileManager.default.fileExists(atPath: plistURL.path)
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
