import Testing
@testable import ConductorRemoteKit

private func relayStatus(pid: Int32, trusted: Bool) -> HostStatus {
    HostStatus(
        version: "0.1.0", pid: pid, startedAt: 1_759_824_000_000, supervisor: "app", port: 8790,
        conductor: .init(running: true), accessibility: .init(trusted: trusted),
        activity: .init(working: 0))
}

@Test func grantWatchFlipsOnceWhenTheAppBecomesTrusted() {
    var watch = GrantWatch()
    let first = watch.observe(appTrusted: false)
    let second = watch.observe(appTrusted: false)
    let granted = watch.observe(appTrusted: true)
    let after = watch.observe(appTrusted: true)
    #expect([first, second, granted, after] == [false, false, true, false])
}

@Test func grantWatchDoesNotFlipWhenTrustedFromTheStart() {
    var watch = GrantWatch()
    let first = watch.observe(appTrusted: true)
    let second = watch.observe(appTrusted: true)
    #expect([first, second] == [false, false])
}

@Test func grantWatchFlipsAgainAfterTheGrantIsLostAndGivenBack() {
    var watch = GrantWatch()
    let denied = watch.observe(appTrusted: false)
    let granted = watch.observe(appTrusted: true)
    let revoked = watch.observe(appTrusted: false)
    let grantedAgain = watch.observe(appTrusted: true)
    #expect([denied, granted, revoked, grantedAgain] == [false, true, false, true])
}

@Test func grantWatchAsksOncePerRelayPidWhenTheRelayLagsBehind() {
    var watch = GrantWatch()
    let stale = relayStatus(pid: 100, trusted: false)
    let first = watch.observe(relay: stale, appTrusted: true)
    let again = watch.observe(relay: stale, appTrusted: true)
    let restarted = watch.observe(relay: relayStatus(pid: 101, trusted: false), appTrusted: true)
    #expect([first, again, restarted] == [true, false, true])
}

@Test func grantWatchLeavesARelayThatAgreesWithTheApp() {
    var watch = GrantWatch()
    let bothTrusted = watch.observe(relay: relayStatus(pid: 100, trusted: true), appTrusted: true)
    let bothUntrusted = watch.observe(relay: relayStatus(pid: 100, trusted: false), appTrusted: false)
    #expect([bothTrusted, bothUntrusted] == [false, false])
}
