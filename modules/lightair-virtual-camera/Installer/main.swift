//  Lightair Virtual Camera: the app that installs (or removes) the camera extension. macOS only
//  installs a camera extension from the app that holds it, from /Applications; once installed,
//  the camera runs on its own and Lightair sends its picture there (virtualcam.m).
//
//  `--uninstall` removes it.

import AppKit
import SystemExtensions

let extensionID = "app.lightair.virtual-camera.extension"

final class Installer: NSObject, NSApplicationDelegate, OSSystemExtensionRequestDelegate {
    private var window: NSWindow!
    private let status = NSTextField(wrappingLabelWithString: "")
    private let detail = NSTextField(wrappingLabelWithString: "")
    private let settingsButton = NSButton(title: "Open System Settings", target: nil, action: nil)
    private let doneButton = NSButton(title: "Done", target: nil, action: nil)
    private let removing = CommandLine.arguments.contains("--uninstall")

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildWindow()
        submit()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    private func buildWindow() {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 440, height: 210),
                          styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "Lightair Virtual Camera"
        window.center()
        let icon = NSImageView(image: NSApp.applicationIconImage)
        icon.translatesAutoresizingMaskIntoConstraints = false
        status.font = .systemFont(ofSize: 15, weight: .semibold)
        detail.font = .systemFont(ofSize: 12)
        detail.textColor = .secondaryLabelColor
        settingsButton.target = self
        settingsButton.action = #selector(openSettings)
        settingsButton.isHidden = true
        doneButton.target = self
        doneButton.action = #selector(done)
        doneButton.keyEquivalent = "\r"
        let text = NSStackView(views: [status, detail])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 6
        let buttons = NSStackView(views: [NSView(), settingsButton, doneButton])
        buttons.orientation = .horizontal
        let row = NSStackView(views: [icon, text])
        row.alignment = .top
        row.spacing = 16
        let column = NSStackView(views: [row, buttons])
        column.orientation = .vertical
        column.alignment = .trailing
        column.spacing = 18
        column.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 20, right: 20)
        column.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(column)
        window.contentView = content
        NSLayoutConstraint.activate([
            icon.widthAnchor.constraint(equalToConstant: 64), icon.heightAnchor.constraint(equalToConstant: 64),
            column.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            column.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            column.topAnchor.constraint(equalTo: content.topAnchor),
            column.bottomAnchor.constraint(lessThanOrEqualTo: content.bottomAnchor),
            row.leadingAnchor.constraint(equalTo: column.leadingAnchor, constant: 20),
            row.trailingAnchor.constraint(equalTo: column.trailingAnchor, constant: -20),
        ])
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private func show(_ title: String, _ more: String, settings: Bool = false) {
        status.stringValue = title
        detail.stringValue = more
        settingsButton.isHidden = !settings
    }

    private func submit() {
        let request = removing
            ? OSSystemExtensionRequest.deactivationRequest(forExtensionWithIdentifier: extensionID, queue: .main)
            : OSSystemExtensionRequest.activationRequest(forExtensionWithIdentifier: extensionID, queue: .main)
        request.delegate = self
        show(removing ? "Removing the virtual camera…" : "Installing the virtual camera…", "")
        OSSystemExtensionManager.shared.submitRequest(request)
    }

    // MARK: OSSystemExtensionRequestDelegate

    func request(_ request: OSSystemExtensionRequest, actionForReplacingExtension existing: OSSystemExtensionProperties,
                 withExtension ext: OSSystemExtensionProperties) -> OSSystemExtensionRequest.ReplacementAction {
        .replace
    }

    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) {
        show("Allow Lightair Virtual Camera",
             "In System Settings › General › Login Items & Extensions › Camera Extensions, switch on Lightair Virtual Camera. This window updates when it's on.",
             settings: true)
    }

    func request(_ request: OSSystemExtensionRequest, didFinishWithResult result: OSSystemExtensionRequest.Result) {
        switch result {
        case .completed where removing:
            show("Removed", "Lightair Virtual Camera is no longer a camera on this Mac.")
        case .completed:
            show("Ready", "In Zoom, Meet, FaceTime or a browser, choose “Lightair Virtual Camera”. Start it from Lightair's title bar.")
        case .willCompleteAfterReboot:
            show("Restart to finish", "macOS finishes the change when this Mac restarts.")
        @unknown default:
            show("Done", "")
        }
    }

    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) {
        show(removing ? "Couldn't remove it" : "Couldn't install it", error.localizedDescription,
             settings: (error as NSError).code == OSSystemExtensionError.authorizationRequired.rawValue)
    }

    @objc private func openSettings() {
        NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!)
    }

    @objc private func done() { NSApp.terminate(nil) }
}

let app = NSApplication.shared
let installer = Installer()
app.delegate = installer
app.setActivationPolicy(.regular)
app.run()
