import Foundation
import Testing
@testable import ConductorRemoteKit

private struct TimedOut: Error {}

private final class FakeLauncher: ProcessLauncher, @unchecked Sendable {
    private let lock = NSLock()
    private var _requests: [LaunchRequest] = []
    private var _timeouts: [Duration] = []
    private let outcome: Result<(status: Int32, output: Data), TimedOut>

    init(status: Int32 = 0, output: String) {
        outcome = .success((status, Data(output.utf8)))
    }

    init(failing: TimedOut) {
        outcome = .failure(failing)
    }

    var requests: [LaunchRequest] { lock.withLock { _requests } }
    var timeouts: [Duration] { lock.withLock { _timeouts } }

    func launch(_ request: LaunchRequest) throws -> any RunningProcess {
        fatalError("tailnet commands never launch a long-running program")
    }

    func run(_ request: LaunchRequest, timeout: Duration) async throws -> (status: Int32, output: Data) {
        lock.withLock {
            _requests.append(request)
            _timeouts.append(timeout)
        }
        return try outcome.get()
    }
}

private let relay = URL(filePath: "/Applications/Test.app/Contents/MacOS/conductor-remote")
private let paths = RelayPaths(home: URL(filePath: "/Users/someone"))

@Test func tailnetCommandRunsTheRelayWithTheActionAndTheRelayEnvironment() async {
    let launcher = FakeLauncher(output: #"{"tailscale":true,"mapped":false}"#)
    _ = await TailnetCommand(relay: relay, paths: paths, launcher: launcher).run(.ensure)
    #expect(launcher.requests == [LaunchRequest(
        executable: relay, arguments: ["tailnet", "ensure", "--json"], environment: paths.relayEnvironment)])
    #expect(launcher.timeouts == [.seconds(20)])
}

@Test func tailnetCommandDecodesAMappedReport() async {
    let json = #"{"tailscale":true,"host":"mac.example.ts.net","httpsPort":8443,"mapped":true,"url":"https://mac.example.ts.net:8443/","error":null}"#
    let launcher = FakeLauncher(output: "warming up\n\(json)\n\n")
    let report = await TailnetCommand(relay: relay, paths: paths, launcher: launcher).run(.status)
    #expect(report == TailnetReport(
        tailscale: true, host: "mac.example.ts.net", httpsPort: 8443, mapped: true,
        url: "https://mac.example.ts.net:8443/"))
}

@Test func tailnetCommandKeepsTheReportOfAFailedRun() async {
    let json = #"{"tailscale":false,"mapped":false,"error":"tailscale is not installed"}"#
    let launcher = FakeLauncher(status: 1, output: json + "\n")
    let report = await TailnetCommand(relay: relay, paths: paths, launcher: launcher).run(.ensure)
    #expect(report == TailnetReport(tailscale: false, mapped: false, error: "tailscale is not installed"))
}

@Test func tailnetCommandReportsGarbageOutput() async {
    let launcher = FakeLauncher(status: 0, output: "this is not json\n")
    let report = await TailnetCommand(relay: relay, paths: paths, launcher: launcher).run(.status)
    #expect(report.tailscale == false)
    #expect(report.mapped == false)
    #expect(report.url == nil)
    #expect(report.error?.isEmpty == false)
}

@Test func tailnetCommandReportsEmptyOutput() async {
    let launcher = FakeLauncher(status: 2, output: " \n\n")
    let report = await TailnetCommand(relay: relay, paths: paths, launcher: launcher).run(.off)
    #expect(report.mapped == false)
    #expect(report.error?.isEmpty == false)
}

@Test func tailnetCommandReportsATimeout() async {
    let launcher = FakeLauncher(failing: TimedOut())
    let report = await TailnetCommand(relay: relay, paths: paths, launcher: launcher).run(.ensure)
    #expect(report.tailscale == false)
    #expect(report.mapped == false)
    #expect(report.error?.isEmpty == false)
}
