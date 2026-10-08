import Foundation
import Observation
import ServiceManagement

/// The app as a login item (`SMAppService.mainApp`). It is on by default: the first launch
/// registers it once.
@MainActor
@Observable
final class LaunchAtLogin {
    private static let registeredOnceKey = "launchAtLoginRegisteredOnce"

    private(set) var isEnabled = false
    /// Why the last change did not take; `nil` when it did.
    private(set) var lastError: String?

    /// The last change asked for; the next one waits for it, so they reach launchd in order.
    @ObservationIgnored private var pending: Task<Void, Never>?
    @ObservationIgnored private var started = false

    /// Reads the status, or on the very first launch registers the app.
    func start() {
        guard !started else { return }
        started = true
        if UserDefaults.standard.bool(forKey: Self.registeredOnceKey) {
            enqueue { await self.refresh() }
        } else {
            UserDefaults.standard.set(true, forKey: Self.registeredOnceKey)
            setEnabled(true)
        }
    }

    /// The toggle: shows the new value at once, then asks launchd.
    func setEnabled(_ enabled: Bool) {
        isEnabled = enabled
        enqueue { await self.apply(enabled) }
    }

    private func enqueue(_ work: @escaping @MainActor () async -> Void) {
        let previous = pending
        pending = Task {
            await previous?.value
            await work()
        }
    }

    private func refresh() async {
        // Every call to the service is a synchronous round trip to launchd.
        isEnabled = await Task.detached { SMAppService.mainApp.status == .enabled }.value
    }

    private func apply(_ enabled: Bool) async {
        isEnabled = enabled
        let failure: String? = await Task.detached {
            let service = SMAppService.mainApp
            do {
                let status = service.status
                if enabled {
                    if status != .enabled { try service.register() }
                } else if status == .enabled || status == .requiresApproval {
                    try service.unregister()
                }
                return nil
            } catch {
                return Self.describe(error)
            }
        }.value
        lastError = failure
        if failure != nil { await refresh() }
    }

    private nonisolated static func describe(_ error: Error) -> String {
        if isUnsupported(error as NSError) {
            return "Launch at login needs the app to be in Applications."
        }
        return "Launch at login could not be changed: \(error.localizedDescription)"
    }

    /// Whether the error, or one under it, says the operation is unsupported.
    private nonisolated static func isUnsupported(_ error: NSError) -> Bool {
        switch (error.domain, error.code) {
        case (NSCocoaErrorDomain, NSFeatureUnsupportedError),
             (NSPOSIXErrorDomain, Int(ENOTSUP)),
             (NSPOSIXErrorDomain, Int(EOPNOTSUPP)):
            return true
        default:
            break
        }
        let text = error.localizedDescription.lowercased()
        if text.contains("unsupported") || text.contains("not supported") { return true }
        if let underlying = error.userInfo[NSUnderlyingErrorKey] as? NSError {
            return isUnsupported(underlying)
        }
        return false
    }
}
