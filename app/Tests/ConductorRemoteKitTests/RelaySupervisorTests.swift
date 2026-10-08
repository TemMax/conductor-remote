import Foundation
import Testing
@testable import ConductorRemoteKit

// MARK: - Fakes

private final class FakeProcess: RunningProcess, @unchecked Sendable {
    let pid: Int32
    private let exitsOnTerminate: Bool
    private let lock = NSLock()
    private var status: Int32?
    private var waiters: [CheckedContinuation<Int32, Never>] = []
    private var log: [String] = []

    init(pid: Int32, exitsOnTerminate: Bool) {
        self.pid = pid
        self.exitsOnTerminate = exitsOnTerminate
    }

    var isRunning: Bool { lock.withLock { status == nil } }
    /// "terminate" and "kill", in the order they reached a running process.
    var signals: [String] { lock.withLock { log } }

    func terminate() {
        guard lock.withLock({ if status == nil { log.append("terminate"); return true } else { return false } }) else { return }
        if exitsOnTerminate { exit(SIGTERM) }
    }

    func kill() {
        guard lock.withLock({ if status == nil { log.append("kill"); return true } else { return false } }) else { return }
        exit(SIGKILL)
    }

    func waitForExit() async -> Int32 {
        await withCheckedContinuation { continuation in
            let ended: Int32? = lock.withLock {
                if status == nil { waiters.append(continuation) }
                return status
            }
            if let ended { continuation.resume(returning: ended) }
        }
    }

    /// Ends the process on its own.
    func exit(_ code: Int32) {
        let pending: [CheckedContinuation<Int32, Never>] = lock.withLock {
            guard status == nil else { return [] }
            status = code
            defer { waiters = [] }
            return waiters
        }
        for continuation in pending { continuation.resume(returning: code) }
    }
}

private final class FakeLauncher: ProcessLauncher, @unchecked Sendable {
    private let lock = NSLock()
    private var launched: [(request: LaunchRequest, process: FakeProcess)] = []
    private let exitsOnTerminate: Bool

    init(exitsOnTerminate: Bool = true) { self.exitsOnTerminate = exitsOnTerminate }

    var requests: [LaunchRequest] { lock.withLock { launched.map(\.request) } }
    var processes: [FakeProcess] { lock.withLock { launched.map(\.process) } }

    func launch(_ request: LaunchRequest) throws -> any RunningProcess {
        lock.withLock {
            let process = FakeProcess(pid: Int32(1000 + launched.count), exitsOnTerminate: exitsOnTerminate)
            launched.append((request, process))
            return process
        }
    }

    func run(_ request: LaunchRequest, timeout: Duration) async throws -> (status: Int32, output: Data) {
        (0, Data())
    }
}

private final class TestClock: Clock, @unchecked Sendable {
    struct Instant: InstantProtocol {
        var offset: Duration
        func advanced(by duration: Duration) -> Instant { Instant(offset: offset + duration) }
        func duration(to other: Instant) -> Duration { other.offset - offset }
        static func < (lhs: Instant, rhs: Instant) -> Bool { lhs.offset < rhs.offset }
    }

    private struct Sleeper {
        var deadline: Instant
        var continuation: CheckedContinuation<Void, Error>
    }

    private let lock = NSLock()
    private var current = Instant(offset: .zero)
    private var sleepers: [UInt64: Sleeper] = [:]
    private var nextID: UInt64 = 0

    var now: Instant { lock.withLock { current } }
    var minimumResolution: Duration { .nanoseconds(1) }

    func sleep(until deadline: Instant, tolerance: Duration?) async throws {
        let id = lock.withLock { defer { nextID += 1 }; return nextID }
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                let outcome: Result<Void, Error>? = lock.withLock {
                    if Task.isCancelled { return .failure(CancellationError()) }
                    if deadline <= current { return .success(()) }
                    sleepers[id] = Sleeper(deadline: deadline, continuation: continuation)
                    return nil
                }
                if let outcome { continuation.resume(with: outcome) }
            }
        } onCancel: {
            let sleeper = lock.withLock { sleepers.removeValue(forKey: id) }
            sleeper?.continuation.resume(throwing: CancellationError())
        }
    }

    /// Moves time forward; sleepers whose deadline has come wake up.
    func advance(by duration: Duration) {
        let due: [Sleeper] = lock.withLock {
            current = current.advanced(by: duration)
            let ids = sleepers.filter { $0.value.deadline <= current }.map(\.key)
            return ids.compactMap { sleepers.removeValue(forKey: $0) }
        }
        for sleeper in due { sleeper.continuation.resume() }
    }

    /// Waits for somebody to sleep, then moves time to the earliest deadline; returns how far.
    @discardableResult
    func advanceToNextSleeper() async -> Duration {
        while true {
            let wait: Duration? = lock.withLock { sleepers.values.map(\.deadline).min().map { current.duration(to: $0) } }
            if let wait {
                advance(by: wait)
                return wait
            }
            try? await Task.sleep(for: .milliseconds(1))
        }
    }
}

private final class StateWatcher {
    private var iterator: AsyncStream<RelayState>.Iterator
    init(_ states: AsyncStream<RelayState>) { iterator = states.makeAsyncIterator() }
    func next() async -> RelayState? { await iterator.next() }
}

private struct Rig {
    let relay = URL(filePath: "/Applications/Conductor Remote.app/Contents/MacOS/conductor-remote")
    let paths = RelayPaths(home: URL(filePath: "/Users/tester", directoryHint: .isDirectory))
    let launcher: FakeLauncher
    let clock = TestClock()
    let supervisor: RelaySupervisor
    let watcher: StateWatcher

    init(exitsOnTerminate: Bool = true, policy: RestartPolicy = .standard) {
        launcher = FakeLauncher(exitsOnTerminate: exitsOnTerminate)
        supervisor = RelaySupervisor(relay: relay, paths: paths, parentPID: 4242, launcher: launcher,
                                     policy: policy, clock: clock)
        watcher = StateWatcher(supervisor.states)
    }

    /// The process the supervisor launched last.
    var latest: FakeProcess { launcher.processes.last! }

    func expect(_ state: RelayState, sourceLocation: SourceLocation = #_sourceLocation) async {
        #expect(await watcher.next() == state, sourceLocation: sourceLocation)
    }
}

// MARK: - Tests

@Test(.timeLimit(.minutes(1))) func launchRequestIsExactlyTheRelaysStartCommand() async {
    let rig = Rig()
    await rig.expect(.stopped)
    await rig.supervisor.start()
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    #expect(rig.launcher.requests == [LaunchRequest(
        executable: rig.relay,
        arguments: ["start", "--exit-with-parent", "--parent-pid", "4242"],
        environment: ["HOME": "/Users/tester", "PATH": RelayPaths.relayPath, "LANG": "en_US.UTF-8"],
        standardOutput: rig.paths.relayLog,
        standardError: rig.paths.relayErrorLog,
        currentDirectory: rig.paths.home)])
    await rig.supervisor.stop()
}

@Test(.timeLimit(.minutes(1))) func startMovesThroughStartingToRunning() async {
    let rig = Rig()
    await rig.expect(.stopped)
    await rig.supervisor.start()
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    #expect(await rig.supervisor.state == .running(pid: 1000))
    await rig.supervisor.stop()
}

@Test(.timeLimit(.minutes(1))) func severalStartsWhileRunningLaunchOnce() async {
    let rig = Rig()
    await rig.supervisor.start()
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    await rig.supervisor.start()
    #expect(rig.launcher.requests.count == 1)
    await rig.supervisor.stop()
    #expect(rig.launcher.requests.count == 1)
}

@Test(.timeLimit(.minutes(1))) func anExitRestartsAfterThePolicysDelays() async {
    let rig = Rig()
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(1), lastStatus: 1))
    #expect(await rig.clock.advanceToNextSleeper() == .seconds(1))
    await rig.expect(.running(pid: 1001))
    rig.latest.exit(2)
    await rig.expect(.restarting(after: .seconds(2), lastStatus: 2))
    #expect(await rig.clock.advanceToNextSleeper() == .seconds(2))
    await rig.expect(.running(pid: 1002))
    #expect(rig.launcher.requests.count == 3)
    await rig.supervisor.stop()
}

@Test(.timeLimit(.minutes(1))) func aHealthyRunResetsTheCount() async {
    let rig = Rig()
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(1), lastStatus: 1))
    await rig.clock.advanceToNextSleeper()
    await rig.expect(.running(pid: 1001))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(2), lastStatus: 1))
    await rig.clock.advanceToNextSleeper()
    await rig.expect(.running(pid: 1002))
    rig.clock.advance(by: RestartPolicy.standard.healthyRun)
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(1), lastStatus: 1))
    await rig.supervisor.stop()
}

@Test(.timeLimit(.minutes(1))) func repeatedQuickFailuresGiveUpAndStartTriesAgain() async {
    let rig = Rig()
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    for (index, delay) in [1, 2, 4, 8, 16].enumerated() {
        await rig.expect(.running(pid: Int32(1000 + index)))
        rig.latest.exit(9)
        await rig.expect(.restarting(after: .seconds(delay), lastStatus: 9))
        await rig.clock.advanceToNextSleeper()
    }
    await rig.expect(.running(pid: 1005))
    rig.latest.exit(9)
    await rig.expect(.failed(lastStatus: 9))
    #expect(rig.launcher.requests.count == 6)

    await rig.supervisor.start()
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1006))
    #expect(rig.launcher.requests.count == 7)
    await rig.supervisor.stop()
}

@Test(.timeLimit(.minutes(1))) func stopTerminatesWaitsAndKillsAProcessThatStays() async {
    let rig = Rig(exitsOnTerminate: false)
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    let process = rig.latest

    let supervisor = rig.supervisor
    let stopping = Task { await supervisor.stop() }
    #expect(await rig.clock.advanceToNextSleeper() == .seconds(5))
    await stopping.value
    #expect(process.signals == ["terminate", "kill"])
    #expect(process.isRunning == false)
    await rig.expect(.stopped)
    #expect(await rig.supervisor.state == .stopped)
    #expect(rig.launcher.requests.count == 1)
}

@Test(.timeLimit(.minutes(1))) func stopDoesNotKillAProcessThatExitsOnTerminate() async {
    let rig = Rig(exitsOnTerminate: true)
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    let process = rig.latest

    await rig.supervisor.stop()
    #expect(process.signals == ["terminate"])
    await rig.expect(.stopped)
    rig.clock.advance(by: .seconds(600))
    #expect(rig.launcher.requests.count == 1)
}

@Test(.timeLimit(.minutes(1))) func stopDuringRestartingLaunchesNothing() async {
    let rig = Rig()
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(1), lastStatus: 1))

    await rig.supervisor.stop()
    await rig.expect(.stopped)
    rig.clock.advance(by: .seconds(600))
    await Task.yield()
    #expect(rig.launcher.requests.count == 1)
    #expect(await rig.supervisor.state == .stopped)
}

@Test(.timeLimit(.minutes(1))) func restartStopsStartsAndResetsTheFailureCount() async {
    let rig = Rig()
    await rig.supervisor.start()
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1000))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(1), lastStatus: 1))
    await rig.clock.advanceToNextSleeper()
    await rig.expect(.running(pid: 1001))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(2), lastStatus: 1))
    await rig.clock.advanceToNextSleeper()
    await rig.expect(.running(pid: 1002))
    let before = rig.latest

    await rig.supervisor.restart()
    #expect(before.signals == ["terminate"])
    await rig.expect(.stopped)
    await rig.expect(.starting)
    await rig.expect(.running(pid: 1003))
    rig.latest.exit(1)
    await rig.expect(.restarting(after: .seconds(1), lastStatus: 1))
    await rig.supervisor.stop()
}
