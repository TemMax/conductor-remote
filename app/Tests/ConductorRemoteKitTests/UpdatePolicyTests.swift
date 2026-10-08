import Testing
@testable import ConductorRemoteKit

private func status(working: Int, idleMs: Int64?) -> HostStatus {
    HostStatus(
        version: "0.1.0", pid: 4242, startedAt: 1_759_824_000_000, supervisor: "app", port: 8790,
        conductor: .init(running: true), accessibility: .init(trusted: true),
        activity: .init(working: working, idleMs: idleMs))
}

@Test func updateMayInstallWhenTheRelayIsNotRunning() {
    #expect(UpdatePolicy.mayInstall(relayRunning: false, status: nil))
}

@Test func runningWithoutAStatusWaits() {
    #expect(!UpdatePolicy.mayInstall(relayRunning: true, status: nil))
}

@Test func updateWaitsWhileConductorIsWorking() {
    #expect(!UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 1, idleMs: nil)))
    #expect(!UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 2, idleMs: 3_600_000)))
}

@Test func updateWaitsForTheQuietPeriod() {
    #expect(UpdatePolicy.quietPeriod == .seconds(600))
    #expect(!UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: 5_300)))
    #expect(!UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: 599_999)))
}

@Test func updateMayInstallAfterTheQuietPeriod() {
    #expect(UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: 600_000)))
    #expect(UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: 3_600_000)))
}

@Test func updateMayInstallWhenNothingHasEverHappened() {
    #expect(UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: nil)))
}

@Test func theThresholdFollowsTheQuietPeriod() {
    let thresholdMs = UpdatePolicy.quietPeriod.components.seconds * 1000
    #expect(!UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: thresholdMs - 1)))
    #expect(UpdatePolicy.mayInstall(relayRunning: true, status: status(working: 0, idleMs: thresholdMs)))
}
