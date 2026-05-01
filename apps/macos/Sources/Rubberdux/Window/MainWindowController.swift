import AppKit

class MainWindowController: NSWindowController {
    convenience init(baseURL: URL) {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1200, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Rubberdux"
        window.center()
        window.setFrameAutosaveName("RubberduxMainWindow")
        self.init(window: window)
        window.contentViewController = MainSplitViewController(baseURL: baseURL)
    }
}
