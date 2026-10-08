import Foundation

/// How soon a relay that ended on its own is started again.
public struct RestartPolicy: Sendable, Equatable {
    /// 1, 2, 4, 8, 16, then 30 seconds.
    public static let standard = RestartPolicy(healthyRun: .seconds(60), maxQuickFailures: 6)

    /// A run at least this long resets the count.
    public var healthyRun: Duration
    /// After this many quick failures in a row the supervisor gives up.
    public var maxQuickFailures: Int

    /// The pause before restart number `attempt` (1-based).
    public func delay(afterAttempt attempt: Int) -> Duration {
        let doublings = min(max(attempt - 1, 0), 5)
        return .seconds(min(1 << doublings, 30))
    }
}
