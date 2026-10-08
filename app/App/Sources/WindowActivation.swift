import AppKit
import SwiftUI

/// The app has no Dock icon (`LSUIElement`). While one of its windows is open it is a regular
/// app, so the window comes to the front and has a menu bar; when the last one closes it is an
/// accessory again.
@MainActor
final class WindowActivation: NSObject {
    static let shared = WindowActivation()

    /// The app's own windows, reported by their content (see `appWindow()`).
    private let windows = NSHashTable<NSWindow>.weakObjects()

    private override init() {
        super.init()
        NotificationCenter.default.addObserver(
            self, selector: #selector(windowWillClose(_:)),
            name: NSWindow.willCloseNotification, object: nil)
    }

    /// Opens one of the app's windows and brings the app to the front.
    func open(_ id: String, with openWindow: OpenWindowAction) {
        NSApp.setActivationPolicy(.regular)
        openWindow(id: id)
        // The app's menu bar is installed when the app becomes active, which it does not in the
        // turn of the run loop that made it a regular app.
        Task { @MainActor in NSApp.activate() }
    }

    /// macOS 14 has no `defaultLaunchBehavior`: closes the windows SwiftUI opened at launch.
    func closeWindowsOpenedAtLaunch() {
        for window in NSApp.windows where window.styleMask.contains(.titled) && !(window is NSPanel) {
            window.close()
        }
    }

    fileprivate func track(_ window: NSWindow) {
        windows.add(window)
    }

    @objc private func windowWillClose(_ notification: Notification) {
        guard let closing = notification.object as? NSWindow, windows.contains(closing) else { return }
        let anotherIsOpen = windows.allObjects.contains { $0 !== closing && $0.isVisible }
        if !anotherIsOpen { NSApp.setActivationPolicy(.accessory) }
    }
}

/// Tells `WindowActivation` which window a view sits in.
private struct WindowReporter: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { ReportingView() }
    func updateNSView(_ nsView: NSView, context: Context) {}

    private final class ReportingView: NSView {
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let window { WindowActivation.shared.track(window) }
        }
    }
}

extension View {
    /// Marks the view as the content of one of the app's windows.
    func appWindow() -> some View {
        background(WindowReporter())
    }
}

extension Scene {
    /// Keeps the scene's window closed at launch. macOS 14 has no such setting; there the
    /// delegate closes what SwiftUI opened.
    func closedAtLaunch() -> some Scene {
        if #available(macOS 15.0, *) {
            return defaultLaunchBehavior(.suppressed)
        } else {
            return self
        }
    }
}
