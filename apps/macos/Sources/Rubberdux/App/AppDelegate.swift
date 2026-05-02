import AppKit

@main
class AppDelegate: NSObject, NSApplicationDelegate {
    var mainWindowController: MainWindowController?
    let backendProvider = BackendProvider()
    private let statusBarController = StatusBarController()
    private var settingsWindowController: SettingsWindowController?

    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        cleanupStaleLaunchAgents()
        statusBarController.onShowWindow = { [weak self] in self?.showMainWindow() }
        statusBarController.onShowSettings = { [weak self] in self?.showSettings() }

        Task {
            do {
                let baseURL = try await backendProvider.ensureAvailable()
                await MainActor.run {
                    mainWindowController = MainWindowController(baseURL: baseURL)
                    mainWindowController?.showWindow(nil)
                    applyAppearanceMode(AppearanceMode.current)
                }
            } catch {
                await MainActor.run {
                    let alert = NSAlert()
                    alert.messageText = "Failed to start backend"
                    alert.informativeText = error.localizedDescription
                    alert.runModal()
                    NSApp.terminate(nil)
                }
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        backendProvider.shutdown()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag {
            showMainWindow()
        }
        return true
    }

    func showMainWindow() {
        mainWindowController?.showWindow(nil)
        mainWindowController?.window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func showSettings() {
        if settingsWindowController == nil {
            settingsWindowController = SettingsWindowController()
        }
        if let vc = settingsWindowController?.window?.contentViewController as? SettingsViewController {
            vc.onAppearanceModeChanged = { [weak self] mode in
                self?.applyAppearanceMode(mode)
            }
        }
        settingsWindowController?.showWindow(nil)
        settingsWindowController?.window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func cleanupStaleLaunchAgents() {
        let staleLabels = [
            "com.wezzard.rubberdux.app.debug",
            "com.wezzard.rubberdux.app.release",
            "com.wezzard.rubberdux.app.dev",
        ]
        let launchAgents = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents")
        for label in staleLabels {
            let plist = launchAgents.appendingPathComponent("\(label).plist")
            if FileManager.default.fileExists(atPath: plist.path) {
                let uid = getuid()
                let process = Process()
                process.executableURL = URL(fileURLWithPath: "/bin/launchctl")
                process.arguments = ["bootout", "gui/\(uid)/\(label)"]
                try? process.run()
                process.waitUntilExit()
                try? FileManager.default.removeItem(at: plist)
            }
        }
    }

    private func applyAppearanceMode(_ mode: AppearanceMode) {
        let previousPolicy = NSApp.activationPolicy()
        let newPolicy = mode.activationPolicy

        if mode.showsStatusItem {
            statusBarController.show()
        } else {
            statusBarController.hide()
        }

        if previousPolicy != newPolicy {
            mainWindowController?.window?.canHide = false

            NSApp.setActivationPolicy(newPolicy)

            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [weak self] in
                NSApp.activate(ignoringOtherApps: true)
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) {
                    self?.mainWindowController?.window?.canHide = true
                }
            }
        }
    }
}
