import AppKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var mainWindowController: MainWindowController?
    let backendProvider = BackendProvider()
    private let statusBarController = StatusBarController()
    private var settingsWindowController: SettingsWindowController?

    func applicationDidFinishLaunching(_ notification: Notification) {
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
