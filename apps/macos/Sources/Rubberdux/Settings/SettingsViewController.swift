import AppKit
import ServiceManagement

final class SettingsViewController: NSViewController {
    var onAppearanceModeChanged: ((AppearanceMode) -> Void)?

    private var radioButtons: [NSButton] = []
    private let loginCheckbox = NSButton(checkboxWithTitle: "Launch at Login",
                                          target: nil, action: nil)

    override func loadView() {
        view = NSView(frame: NSRect(x: 0, y: 0, width: 360, height: 220))

        let showInLabel = NSTextField(labelWithString: "Show in:")
        showInLabel.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .medium)
        showInLabel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(showInLabel)

        let modes: [(AppearanceMode, String)] = [
            (.dockAndMenuBar, "Dock and menu bar"),
            (.menuBarOnly, "Menu bar only"),
            (.dockOnly, "Dock only"),
        ]

        var previousAnchor = showInLabel.bottomAnchor
        for (mode, title) in modes {
            let radio = NSButton(radioButtonWithTitle: title,
                                  target: self, action: #selector(radioChanged(_:)))
            radio.tag = mode.rawValue
            radio.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(radio)
            radioButtons.append(radio)

            NSLayoutConstraint.activate([
                radio.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
                radio.topAnchor.constraint(equalTo: previousAnchor, constant: 6),
            ])
            previousAnchor = radio.bottomAnchor
        }

        loginCheckbox.target = self
        loginCheckbox.action = #selector(loginCheckboxChanged(_:))
        loginCheckbox.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(loginCheckbox)

        NSLayoutConstraint.activate([
            showInLabel.topAnchor.constraint(equalTo: view.topAnchor, constant: 20),
            showInLabel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
            loginCheckbox.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
            loginCheckbox.topAnchor.constraint(equalTo: previousAnchor, constant: 16),
        ])

        if !BuildType.current.supportsLaunchAtLogin {
            loginCheckbox.isEnabled = false
            let hint = NSTextField(labelWithString:
                "Available when Rubberdux is installed as an application.")
            hint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
            hint.textColor = .secondaryLabelColor
            hint.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(hint)
            NSLayoutConstraint.activate([
                hint.leadingAnchor.constraint(equalTo: loginCheckbox.leadingAnchor, constant: 18),
                hint.topAnchor.constraint(equalTo: loginCheckbox.bottomAnchor, constant: 2),
            ])
        }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let current = AppearanceMode.current
        for radio in radioButtons {
            radio.state = radio.tag == current.rawValue ? .on : .off
        }
        if BuildType.current.supportsLaunchAtLogin {
            loginCheckbox.state = SMAppService.mainApp.status == .enabled ? .on : .off
        }
    }

    @objc private func radioChanged(_ sender: NSButton) {
        guard let mode = AppearanceMode(rawValue: sender.tag) else { return }
        AppearanceMode.save(mode)
        onAppearanceModeChanged?(mode)
    }

    @objc private func loginCheckboxChanged(_ sender: NSButton) {
        do {
            if sender.state == .on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            sender.state = sender.state == .on ? .off : .on
        }
    }
}
