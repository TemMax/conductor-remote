import Foundation
import Testing
@testable import ConductorRemoteKit

private final class FakeLauncher: ProcessLauncher, @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [(LaunchRequest, Duration)] = []
    let status: Int32
    init(status: Int32 = 0) { self.status = status }

    func launch(_ request: LaunchRequest) throws -> any RunningProcess { fatalError("not used") }

    func run(_ request: LaunchRequest, timeout: Duration) async throws -> (status: Int32, output: Data) {
        lock.withLock { requests.append((request, timeout)) }
        return (status, Data())
    }

    func calls() -> [(LaunchRequest, Duration)] { lock.withLock { requests } }
}

private let relay = URL(fileURLWithPath: "/Applications/Conductor Remote.app/Contents/MacOS/conductor-remote")

private func makeHome() -> RelayPaths {
    RelayPaths(home: FileManager.default.temporaryDirectory.appending(path: "legacy-\(UUID().uuidString)"))
}

@Test func legacyServiceIsInstalledWhenThePlistExists() throws {
    let paths = makeHome()
    let service = LegacyService(paths: paths, relay: relay, launcher: FakeLauncher())
    #expect(!service.isInstalled)
    try FileManager.default.createDirectory(at: paths.legacyLaunchAgent.deletingLastPathComponent(),
                                            withIntermediateDirectories: true)
    try Data("<plist/>".utf8).write(to: paths.legacyLaunchAgent)
    #expect(service.isInstalled)
}

@Test func removeRunsServiceUninstall() async throws {
    let paths = makeHome()
    let launcher = FakeLauncher()
    try await LegacyService(paths: paths, relay: relay, launcher: launcher).remove()
    let calls = launcher.calls()
    #expect(calls.count == 1)
    let (request, timeout) = try #require(calls.first)
    #expect(request.executable == relay)
    #expect(request.arguments == ["service", "uninstall"])
    #expect(request.environment == paths.relayEnvironment)
    #expect(timeout == .seconds(30))
}

@Test func failingUninstallThrows() async {
    let service = LegacyService(paths: makeHome(), relay: relay, launcher: FakeLauncher(status: 3))
    await #expect(throws: LegacyServiceError.failed(status: 3)) { try await service.remove() }
}
