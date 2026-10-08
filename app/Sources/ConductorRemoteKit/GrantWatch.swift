import Foundation

/// Decides when the relay must be restarted for the Accessibility grant: the grant reaches a
/// process only when it starts, so a relay that started before the grant has to be restarted.
public struct GrantWatch: Sendable, Equatable {
    private var appWasTrusted: Bool?
    private var restartedPids: Set<Int32> = []

    public init() {}

    /// Feeds the app's own `AXIsProcessTrusted()`; `true` exactly once when it turns from false
    /// to true after having been false: the relay must be restarted.
    public mutating func observe(appTrusted: Bool) -> Bool {
        defer { appWasTrusted = appTrusted }
        return appWasTrusted == false && appTrusted
    }

    /// Whether the relay should be restarted because it still says untrusted while the app is
    /// trusted (a relay started before the grant): `true` once per relay pid.
    public mutating func observe(relay: HostStatus, appTrusted: Bool) -> Bool {
        guard appTrusted, !relay.accessibility.trusted else { return false }
        return restartedPids.insert(relay.pid).inserted
    }
}
