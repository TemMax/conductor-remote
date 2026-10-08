import Foundation
import Testing
@testable import ConductorRemoteKit

private func hostStatus(supervisor: String, pid: Int32 = 777) -> HostStatus {
    HostStatus(version: "0.1.0", pid: pid, startedAt: 0, supervisor: supervisor, port: 8790,
               conductor: .init(running: true), accessibility: .init(trusted: true),
               activity: .init(working: 0))
}

private final class Recorder: @unchecked Sendable {
    private let lock = NSLock()
    private var signalled: [Int32] = []
    private var freeCalls = 0
    func signal(_ pid: Int32) { lock.withLock { signalled.append(pid) } }
    func signals() -> [Int32] { lock.withLock { signalled } }
    func nextFreeCall() -> Int { lock.withLock { freeCalls += 1; return freeCalls } }
}

private let relay = URL(fileURLWithPath: "/Applications/Conductor Remote.app/Contents/MacOS/conductor-remote")

private func guardWith(_ recorder: Recorder, status: @escaping @Sendable () async throws -> HostStatus,
                       path: String? = relay.path,
                       portFree: @escaping @Sendable () async -> Bool = { true }) -> OrphanGuard {
    OrphanGuard(relay: relay, status: status, processPath: { _ in path },
                signal: { recorder.signal($0) }, portFree: portFree)
}

@Test func freePortWhenNothingAnswers() async {
    let recorder = Recorder()
    let owner = await guardWith(recorder, status: { throw StatusError.unreachable }).check()
    #expect(owner == .free)
    #expect(recorder.signals().isEmpty)
}

@Test func launchdRelayIsTheLegacyService() async {
    let recorder = Recorder()
    let owner = await guardWith(recorder, status: { hostStatus(supervisor: "launchd") }).check()
    #expect(owner == .legacyService)
    #expect(recorder.signals().isEmpty)
}

@Test func ownOrphanIsStopped() async {
    let recorder = Recorder()
    let owner = await guardWith(recorder, status: { hostStatus(supervisor: "app", pid: 555) },
                                portFree: { recorder.nextFreeCall() >= 3 }).check()
    #expect(owner == .stoppedOrphan(pid: 555))
    #expect(recorder.signals() == [555])
}

@Test func ownOrphanThatDoesNotStopIsTaken() async {
    let recorder = Recorder()
    let owner = await guardWith(recorder, status: { hostStatus(supervisor: "app", pid: 555) },
                                portFree: { false }).check()
    #expect(owner == .taken("the old relay did not stop"))
    #expect(recorder.signals() == [555])
}

@Test func foreignPathIsNeverSignalled() async {
    for path in ["/usr/local/bin/conductor-remote", nil] {
        let recorder = Recorder()
        let owner = await guardWith(recorder, status: { hostStatus(supervisor: "app") }, path: path).check()
        guard case .taken = owner else {
            Issue.record("expected taken, got \(owner)")
            continue
        }
        #expect(recorder.signals().isEmpty)
    }
}

@Test func handStartedRelayIsLeftAlone() async {
    let recorder = Recorder()
    let owner = await guardWith(recorder, status: { hostStatus(supervisor: "none") }).check()
    guard case .taken = owner else { Issue.record("expected taken, got \(owner)"); return }
    #expect(recorder.signals().isEmpty)
}

@Test func anotherTokenIsTaken() async {
    let recorder = Recorder()
    let owner = await guardWith(recorder, status: { throw StatusError.http(401) }).check()
    guard case .taken = owner else { Issue.record("expected taken, got \(owner)"); return }
    #expect(recorder.signals().isEmpty)
}

@Test func symlinkedPathMatchesTheRelay() async throws {
    let directory = FileManager.default.temporaryDirectory.appending(path: "orphan-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    let real = directory.appending(path: "real")
    try Data().write(to: real)
    let link = directory.appending(path: "link")
    try FileManager.default.createSymbolicLink(at: link, withDestinationURL: real)
    let recorder = Recorder()
    let orphanGuard = OrphanGuard(relay: link, status: { hostStatus(supervisor: "app", pid: 9) },
                                  processPath: { _ in real.path }, signal: { recorder.signal($0) },
                                  portFree: { true })
    #expect(await orphanGuard.check() == .stoppedOrphan(pid: 9))
}
