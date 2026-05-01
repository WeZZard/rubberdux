import AppKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var mainWindowController: MainWindowController?
    let backendProvider = BackendProvider()

    func applicationDidFinishLaunching(_ notification: Notification) {
        Task {
            do {
                let baseURL = try await backendProvider.ensureAvailable()
                await MainActor.run {
                    mainWindowController = MainWindowController(baseURL: baseURL)
                    mainWindowController?.showWindow(nil)
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
        true
    }
}
