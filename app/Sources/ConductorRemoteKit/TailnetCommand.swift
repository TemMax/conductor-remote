import Foundation

/// Runs `relay tailnet <action> --json` and reads the report it prints.
public struct TailnetCommand: Sendable {
    public enum Action: String, Sendable {
        case status, ensure, off, enable, disable
    }

    static let timeout: Duration = .seconds(20)

    private let relay: URL
    private let paths: RelayPaths
    private let launcher: any ProcessLauncher

    public init(relay: URL, paths: RelayPaths, launcher: any ProcessLauncher) {
        self.relay = relay
        self.paths = paths
        self.launcher = launcher
    }

    public func reconcile() async -> TailnetReport {
        let report = await run(.status)
        guard report.error == nil, let enabled = report.enabled else { return report }
        if report.tailscale, enabled ? !report.mapped : report.mapped {
            return await run(.ensure)
        }
        return report
    }

    /// The report on the last non-empty output line, whatever the exit status; a run that fails
    /// or prints nothing decodable gives a report that names what failed.
    public func run(_ action: Action) async -> TailnetReport {
        let request = LaunchRequest(
            executable: relay,
            arguments: ["tailnet", action.rawValue, "--json"],
            environment: paths.relayEnvironment)
        let result: (status: Int32, output: Data)
        do {
            result = try await launcher.run(request, timeout: Self.timeout)
        } catch {
            return Self.failure("tailnet \(action.rawValue) failed: \(error)")
        }
        let text = String(decoding: result.output, as: UTF8.self)
        let lastLine = text.split(whereSeparator: \.isNewline)
            .last { !$0.allSatisfy(\.isWhitespace) }
        guard let lastLine else {
            return Self.failure("tailnet \(action.rawValue) printed nothing (exit \(result.status))")
        }
        guard let report = try? JSONDecoder().decode(TailnetReport.self, from: Data(lastLine.utf8)) else {
            return Self.failure("tailnet \(action.rawValue) printed no report (exit \(result.status))")
        }
        return report
    }

    private static func failure(_ message: String) -> TailnetReport {
        TailnetReport(tailscale: false, host: nil, httpsPort: nil, mapped: false, url: nil,
                      error: message, enabled: nil)
    }
}
