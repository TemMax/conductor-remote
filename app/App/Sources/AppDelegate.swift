import AppKit
import ConductorRemoteKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        if #available(macOS 15.0, *) {
            // The windows are kept closed by `defaultLaunchBehavior(.suppressed)`.
            AppModel.shared?.launchFinished()
        } else {
            WindowActivation.shared.closeWindowsOpenedAtLaunch()
            // Once more a turn later, for a window SwiftUI opens after this call.
            Task { @MainActor in
                WindowActivation.shared.closeWindowsOpenedAtLaunch()
                AppModel.shared?.launchFinished()
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Quit stops the relay and waits for it. If the app is killed instead, the relay stops by
    /// itself (`--exit-with-parent`).
    func applicationWillTerminate(_ notification: Notification) {
        guard let supervisor = AppModel.shared?.supervisor else { return }
        let semaphore = DispatchSemaphore(value: 0)
        Task.detached {
            await supervisor.stop()
            semaphore.signal()
        }
        // The supervisor is an actor off the main thread, so this wait cannot deadlock.
        _ = semaphore.wait(timeout: .now() + .seconds(6))
    }
}
