import Foundation

public enum RelayState: Sendable, Equatable {
    case stopped
    case starting
    case running(pid: Int32)
    /// Ended on its own; started again after `in`.
    case restarting(after: Duration, lastStatus: Int32)
    /// Gave up after too many quick failures; `start()` tries again.
    case failed(lastStatus: Int32)
}

/// Keeps the relay running: starts it, starts it again when it ends on its own, stops it on request.
public actor RelaySupervisor {
    /// How long a stopping relay gets between SIGTERM and SIGKILL.
    private static let gracePeriod: Duration = .seconds(5)

    private let relay: URL
    private let paths: RelayPaths
    private let parentPID: Int32
    private let launcher: any ProcessLauncher
    private let policy: RestartPolicy
    private let clock: any Clock<Duration>

    private let continuation: AsyncStream<RelayState>.Continuation
    /// Every change of `state`, the current one first.
    public nonisolated let states: AsyncStream<RelayState>

    public private(set) var state: RelayState = .stopped {
        didSet {
            if state != oldValue { continuation.yield(state) }
        }
    }

    /// The loop that launches, waits and relaunches; `nil` when nothing is supervised.
    private var runner: Task<Void, Never>?
    private var process: (any RunningProcess)?
    private var stopping = false

    public init(relay: URL, paths: RelayPaths, parentPID: Int32, launcher: any ProcessLauncher,
                policy: RestartPolicy = .standard, clock: any Clock<Duration> = ContinuousClock()) {
        self.relay = relay
        self.paths = paths
        self.parentPID = parentPID
        self.launcher = launcher
        self.policy = policy
        self.clock = clock
        let (stream, continuation) = AsyncStream.makeStream(of: RelayState.self)
        self.states = stream
        self.continuation = continuation
        continuation.yield(.stopped)
    }

    deinit {
        continuation.finish()
    }

    /// Starts the relay unless it runs.
    public func start() {
        guard runner == nil, !stopping else { return }
        state = .starting
        runner = Task { await self.supervise() }
    }

    /// Stops the relay: SIGTERM, up to 5 s for it to exit, then SIGKILL. No restart follows.
    public func stop() async {
        guard !stopping else { return }
        stopping = true
        defer { stopping = false }
        let runner = self.runner
        self.runner = nil
        runner?.cancel()
        if let runner {
            if let process {
                process.terminate()
                await Self.wait(for: runner, killing: process, after: Self.gracePeriod, clock: clock)
            } else {
                await runner.value
            }
        }
        state = .stopped
    }

    /// `stop()` then `start()`, resetting the failure count (after the Accessibility grant flips).
    public func restart() async {
        await stop()
        start()
    }

    private var request: LaunchRequest {
        LaunchRequest(
            executable: relay,
            arguments: ["start", "--exit-with-parent", "--parent-pid", "\(parentPID)"],
            environment: paths.relayEnvironment,
            standardOutput: paths.relayLog,
            standardError: paths.relayErrorLog,
            currentDirectory: paths.home)
    }

    private func supervise() async {
        var attempt = 0
        while !Task.isCancelled {
            // A relay that cannot be started counts as a quick failure.
            var status: Int32 = -1
            var ranFor = Duration.zero
            if let running = try? launcher.launch(request) {
                process = running
                ranFor = await clock.measure {
                    state = .running(pid: running.pid)
                    status = await running.waitForExit()
                }
                process = nil
            }
            if Task.isCancelled { return }

            let healthy = ranFor >= policy.healthyRun
            attempt = healthy ? 1 : attempt + 1
            if !healthy, attempt >= policy.maxQuickFailures {
                state = .failed(lastStatus: status)
                runner = nil
                return
            }
            let delay = policy.delay(afterAttempt: attempt)
            state = .restarting(after: delay, lastStatus: status)
            do { try await clock.sleep(for: delay) } catch { return }
        }
    }

    /// Waits for the supervising loop to end, which it does once the process is gone; kills the
    /// process when that takes longer than `grace`.
    private static func wait(for runner: Task<Void, Never>, killing process: any RunningProcess,
                             after grace: Duration, clock: any Clock<Duration>) async {
        await withTaskGroup(of: Void.self) { group in
            group.addTask { await runner.value }
            group.addTask {
                do { try await clock.sleep(for: grace) } catch { return }
                process.kill()
            }
            await group.next()
            group.cancelAll()
        }
    }
}
