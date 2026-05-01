import Foundation

final class BackendProvider {
    enum Error: Swift.Error, LocalizedError {
        case binaryNotFound
        case launchFailed(String)
        case launchTimeout

        var errorDescription: String? {
            switch self {
            case .binaryNotFound:
                return "Could not find the rubberdux binary. Set RUBBERDUX_BIN or build with 'cargo build'."
            case .launchFailed(let reason):
                return "Failed to launch rubberdux: \(reason)"
            case .launchTimeout:
                return "Rubberdux backend did not become ready within 10 seconds."
            }
        }
    }

    private let port: UInt16
    private var process: Process?

    init(port: UInt16 = 19385) {
        self.port = port
    }

    var baseURL: URL {
        URL(string: "http://localhost:\(port)")!
    }

    func ensureAvailable() async throws -> URL {
        if await isHealthy() {
            return baseURL
        }
        let binary = try BinaryLocator.locate()
        try launchProcess(binary: binary)
        try await pollUntilHealthy(timeout: 10.0)
        return baseURL
    }

    func shutdown() {
        if let process, process.isRunning {
            process.terminate()
        }
        process = nil
    }

    // MARK: - Private

    private func isHealthy() async -> Bool {
        let url = baseURL.appendingPathComponent("/api/v1/health")
        do {
            let (data, response) = try await URLSession.shared.data(from: url)
            guard let httpResponse = response as? HTTPURLResponse,
                  httpResponse.statusCode == 200 else {
                return false
            }
            let json = try JSONSerialization.jsonObject(with: data) as? [String: Any]
            return json?["status"] as? String == "ok"
        } catch {
            return false
        }
    }

    private func launchProcess(binary: URL) throws {
        let proc = Process()
        proc.executableURL = binary
        proc.arguments = ["--host"]

        // Forward relevant environment variables
        var env = ProcessInfo.processInfo.environment
        env["RUBBERDUX_GATEWAY_PORT"] = String(port)
        proc.environment = env

        // Set working directory to the workspace root (parent of the binary's target dir)
        // binary is at .../target/{profile}/rubberdux, workspace is 3 levels up
        let workingDir = binary
            .deletingLastPathComponent()  // target/{profile}/
            .deletingLastPathComponent()  // target/
            .deletingLastPathComponent()  // workspace root
        if FileManager.default.fileExists(atPath: workingDir.appendingPathComponent("Cargo.toml").path) {
            proc.currentDirectoryURL = workingDir
        }

        do {
            try proc.run()
            self.process = proc
        } catch {
            throw Error.launchFailed(error.localizedDescription)
        }
    }

    private func pollUntilHealthy(timeout: TimeInterval) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await isHealthy() { return }
            try await Task.sleep(nanoseconds: 200_000_000) // 200ms
        }
        throw Error.launchTimeout
    }
}
