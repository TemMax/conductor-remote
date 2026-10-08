import Foundation

public enum LegacyServiceError: Error, Equatable {
    /// `relay service uninstall` exited with this status.
    case failed(status: Int32)
}

/// The LaunchAgent an earlier version installed.
public struct LegacyService: Sendable {
    private let paths: RelayPaths
    private let relay: URL
    private let launcher: any ProcessLauncher

    public init(paths: RelayPaths, relay: URL, launcher: any ProcessLauncher) {
        self.paths = paths
        self.relay = relay
        self.launcher = launcher
    }

    public var isInstalled: Bool {
        FileManager.default.fileExists(atPath: paths.legacyLaunchAgent.path)
    }

    /// Runs `relay service uninstall`.
    public func remove() async throws {
        let request = LaunchRequest(executable: relay, arguments: ["service", "uninstall"],
                                    environment: paths.relayEnvironment)
        let result = try await launcher.run(request, timeout: .seconds(30))
        guard result.status == 0 else { throw LegacyServiceError.failed(status: result.status) }
    }
}
