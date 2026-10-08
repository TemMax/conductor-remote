import Combine
import ConductorRemoteKit
import Foundation
import Sparkle

/// Sparkle for the app's life. A downloaded update is installed only while the relay is idle.
@MainActor
final class UpdateController: NSObject, ObservableObject, SPUUpdaterDelegate, SPUStandardUserDriverDelegate {
    /// How often a downloaded update asks whether it may be installed.
    private static let idleCheckInterval: TimeInterval = 30

    @Published private(set) var canCheckForUpdates = false
    /// "Install updates automatically": Sparkle's `automaticallyDownloadsUpdates`, both ways.
    @Published var installAutomatically = true {
        didSet {
            // Only a change made here is written: a value Sparkle reported is already its own.
            if updater.automaticallyDownloadsUpdates != installAutomatically {
                updater.automaticallyDownloadsUpdates = installAutomatically
            }
        }
    }

    private let status: @MainActor () -> (relayRunning: Bool, status: HostStatus?)
    private lazy var controller = SPUStandardUpdaterController(
        startingUpdater: true, updaterDelegate: self, userDriverDelegate: self)
    /// Installs the downloaded update and relaunches the app; `nil` until one is downloaded.
    private var installNow: (() -> Void)?
    private var idleTimer: Timer?

    private var updater: SPUUpdater { controller.updater }

    /// `status` is whether the relay runs and its latest status, `nil` when none was read.
    init(status: @escaping @MainActor () -> (relayRunning: Bool, status: HostStatus?)) {
        self.status = status
        super.init()
        updater.publisher(for: \.canCheckForUpdates).assign(to: &$canCheckForUpdates)
        updater.publisher(for: \.automaticallyDownloadsUpdates).assign(to: &$installAutomatically)
    }

    func checkForUpdates() {
        updater.checkForUpdates()
    }

    // MARK: - SPUUpdaterDelegate

    /// Takes over the install: the update waits for the relay to be idle. Sparkle still installs
    /// it when the app quits first.
    func updater(
        _ updater: SPUUpdater, willInstallUpdateOnQuit item: SUAppcastItem,
        immediateInstallationBlock immediateInstallHandler: @escaping () -> Void
    ) -> Bool {
        installNow = immediateInstallHandler
        if idleTimer == nil {
            idleTimer = Timer.scheduledTimer(withTimeInterval: Self.idleCheckInterval, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated { self?.installIfIdle() }
            }
        }
        return true
    }

    /// Sparkle quits the app, which stops the relay, installs the update and relaunches the app;
    /// the relaunched app starts the new relay.
    private func installIfIdle() {
        let current = status()
        guard let installNow,
            UpdatePolicy.mayInstall(relayRunning: current.relayRunning, status: current.status)
        else { return }
        idleTimer?.invalidate()
        idleTimer = nil
        self.installNow = nil
        installNow()
    }

    // MARK: - SPUStandardUserDriverDelegate

    nonisolated var supportsGentleScheduledUpdateReminders: Bool { true }
}
