import Foundation

enum BinaryLocator {
    static func locate() throws -> URL {
        let selfPath = resolvedSelfExecutablePath()

        if let envPath = ProcessInfo.processInfo.environment["RUBBERDUX_BIN"] {
            let url = URL(fileURLWithPath: envPath)
            if isUsableBackend(url, selfPath: selfPath) {
                return url
            }
        }

        if let workspacePath = Bundle.main.infoDictionary?["RubberduxWorkspaceRoot"] as? String,
           !workspacePath.isEmpty {
            let workspace = URL(fileURLWithPath: workspacePath)
            for profile in preferredProfiles() {
                let candidate = workspace
                    .appendingPathComponent("target")
                    .appendingPathComponent(profile)
                    .appendingPathComponent("rubberduxd")
                if isUsableBackend(candidate, selfPath: selfPath) {
                    return candidate
                }
            }
        }

        if let workspace = findWorkspaceRoot() {
            for profile in preferredProfiles() {
                let candidate = workspace
                    .appendingPathComponent("target")
                    .appendingPathComponent(profile)
                    .appendingPathComponent("rubberduxd")
                if isUsableBackend(candidate, selfPath: selfPath) {
                    return candidate
                }
            }
        }

        throw BackendProvider.Error.binaryNotFound
    }

    private static func preferredProfiles() -> [String] {
        switch BuildType.current {
        case .debug: return ["debug", "release"]
        case .release, .distribute: return ["release", "debug"]
        }
    }

    private static func isUsableBackend(_ url: URL, selfPath: String) -> Bool {
        let resolved = url.resolvingSymlinksInPath().path
        guard FileManager.default.isExecutableFile(atPath: resolved) else { return false }
        return resolved != selfPath
    }

    private static func resolvedSelfExecutablePath() -> String {
        URL(fileURLWithPath: CommandLine.arguments[0])
            .resolvingSymlinksInPath()
            .path
    }

    private static func findWorkspaceRoot() -> URL? {
        let startURL = URL(fileURLWithPath: CommandLine.arguments[0])
            .resolvingSymlinksInPath()
            .deletingLastPathComponent()
        var current = startURL
        let fs = FileManager.default
        for _ in 0..<20 {
            let cargoToml = current.appendingPathComponent("Cargo.toml")
            if fs.fileExists(atPath: cargoToml.path) {
                return current
            }
            let parent = current.deletingLastPathComponent()
            if parent.path == current.path { break }
            current = parent
        }
        return nil
    }
}
