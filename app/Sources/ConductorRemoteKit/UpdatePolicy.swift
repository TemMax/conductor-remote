import Foundation

/// When an update may be installed without cutting into the user's work.
public enum UpdatePolicy {
    /// How long Conductor must have been idle before an update may install.
    public static let quietPeriod: Duration = .seconds(600)

    /// `true` when the relay is not running. When it runs, an unknown status is not idle, so the
    /// answer is `false` without one; with one, `true` only when nothing is working and the last
    /// activity is at least the quiet period ago (or unknown).
    public static func mayInstall(relayRunning: Bool, status: HostStatus?) -> Bool {
        guard relayRunning else { return true }
        guard let status else { return false }
        guard status.activity.working == 0 else { return false }
        guard let idleMs = status.activity.idleMs else { return true }
        return idleMs >= quietPeriod.components.seconds * 1000
    }
}
