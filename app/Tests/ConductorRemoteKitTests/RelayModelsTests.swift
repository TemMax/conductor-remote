import Foundation
import Testing
@testable import ConductorRemoteKit

@Test func hostStatusDecodesTheRelaysAnswer() throws {
    let json = """
    {
      "version": "0.1.0",
      "pid": 4242,
      "startedAt": 1759824000000,
      "supervisor": "app",
      "port": 8790,
      "conductor": { "running": true },
      "accessibility": { "trusted": true },
      "screenLocked": false,
      "activity": { "working": 1, "idleMs": 5300 }
    }
    """
    let status = try JSONDecoder().decode(HostStatus.self, from: Data(json.utf8))
    #expect(status == HostStatus(
        version: "0.1.0", pid: 4242, startedAt: 1_759_824_000_000, supervisor: "app", port: 8790,
        conductor: .init(running: true), accessibility: .init(trusted: true), screenLocked: false,
        activity: .init(working: 1, idleMs: 5300)))
}

@Test func hostStatusDecodesNulls() throws {
    let json = """
    {
      "version": "0.1.0",
      "pid": 4242,
      "startedAt": 1759824000000,
      "supervisor": "none",
      "port": 8790,
      "conductor": { "running": false },
      "accessibility": { "trusted": false },
      "screenLocked": null,
      "activity": { "working": 0, "idleMs": null }
    }
    """
    let status = try JSONDecoder().decode(HostStatus.self, from: Data(json.utf8))
    #expect(status == HostStatus(
        version: "0.1.0", pid: 4242, startedAt: 1_759_824_000_000, supervisor: "none", port: 8790,
        conductor: .init(running: false), accessibility: .init(trusted: false),
        activity: .init(working: 0)))
    #expect(status.screenLocked == nil)
    #expect(status.activity.idleMs == nil)
}

@Test func tailnetReportDecodesAMapping() throws {
    let json = """
    {"tailscale":true,"host":"mac.example.ts.net","httpsPort":8443,"mapped":true,"url":"https://mac.example.ts.net:8443/","error":null,"enabled":true}
    """
    let report = try JSONDecoder().decode(TailnetReport.self, from: Data(json.utf8))
    #expect(report == TailnetReport(
        tailscale: true, host: "mac.example.ts.net", httpsPort: 8443, mapped: true,
        url: "https://mac.example.ts.net:8443/"))
    #expect(report.error == nil)
}

@Test func tailnetReportDecodesNulls() throws {
    let json = """
    {"tailscale":false,"host":null,"httpsPort":null,"mapped":false,"url":null,"error":"tailscale is not installed","enabled":true}
    """
    let report = try JSONDecoder().decode(TailnetReport.self, from: Data(json.utf8))
    #expect(report == TailnetReport(tailscale: false, mapped: false, error: "tailscale is not installed"))
    #expect(report.host == nil)
    #expect(report.httpsPort == nil)
    #expect(report.url == nil)
}

@Test func relayPathsFollowTheHome() {
    let paths = RelayPaths(home: URL(filePath: "/Users/someone", directoryHint: .isDirectory))
    let support = "/Users/someone/Library/Application Support/com.temmax.conductor-remote"
    let logs = "/Users/someone/Library/Logs/com.temmax.conductor-remote"
    #expect(paths.stateDirectory.path == support)
    #expect(paths.tokenFile.path == "\(support)/token")
    #expect(paths.settingsFile.path == "\(support)/settings.json")
    #expect(paths.logDirectory.path == logs)
    #expect(paths.relayLog.path == "\(logs)/relay.log")
    #expect(paths.relayErrorLog.path == "\(logs)/relay.err.log")
    #expect(paths.legacyLaunchAgent.path == "/Users/someone/Library/LaunchAgents/com.temmax.conductor-remote.plist")
    #expect(paths.relayEnvironment == [
        "HOME": "/Users/someone",
        "PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
        "LANG": "en_US.UTF-8",
    ])
    #expect(RelayPaths.relayExecutable(inBundle: URL(filePath: "/Applications/Conductor Remote.app")).path
        == "/Applications/Conductor Remote.app/Contents/MacOS/conductor-remote")
    #expect(RelayPaths.defaultPort == 8790)
}
