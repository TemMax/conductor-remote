import Darwin
import Foundation

/// Who holds the relay's port.
public enum PortOwner: Sendable, Equatable {
    /// Nothing answers: the app may start its relay.
    case free
    /// A relay an earlier run of this app started and left behind; it was stopped.
    case stoppedOrphan(pid: Int32)
    /// The old LaunchAgent's relay: replace it through `LegacyService`.
    case legacyService
    /// Something else: a relay started by hand, or another program. Left alone.
    case taken(String)
}

/// Looks at the relay's port before the app starts its own relay.
public struct OrphanGuard: Sendable {
    private let relay: URL
    private let status: @Sendable () async throws -> HostStatus
    private let processPath: @Sendable (Int32) -> String?
    private let signal: @Sendable (Int32) -> Void
    private let portFree: @Sendable () async -> Bool

    public init(relay: URL, status: @escaping @Sendable () async throws -> HostStatus,
                processPath: @escaping @Sendable (Int32) -> String? = OrphanGuard.processPath,
                signal: @escaping @Sendable (Int32) -> Void = { Darwin.kill($0, SIGTERM) },
                portFree: @escaping @Sendable () async -> Bool) {
        self.relay = relay
        self.status = status
        self.processPath = processPath
        self.signal = signal
        self.portFree = portFree
    }

    public func check() async -> PortOwner {
        let answer: HostStatus
        do {
            answer = try await status()
        } catch StatusError.unreachable {
            return .free
        } catch StatusError.http(401) {
            return .taken("a relay with another token answers on the port")
        } catch StatusError.http(let code) {
            return .taken("the port answers with HTTP \(code)")
        } catch StatusError.invalid {
            return .taken("another program answers on the port")
        } catch {
            return .taken("the port could not be checked")
        }

        switch answer.supervisor {
        case "launchd":
            return .legacyService
        case "app":
            guard let path = processPath(answer.pid),
                  URL(fileURLWithPath: path).resolvingSymlinksInPath().path
                    == relay.resolvingSymlinksInPath().path
            else { return .taken("a relay started by another app answers on the port") }
            signal(answer.pid)
            for attempt in 0...20 {
                if await portFree() { return .stoppedOrphan(pid: answer.pid) }
                if attempt < 20 { try? await Task.sleep(for: .milliseconds(250)) }
            }
            return .taken("the old relay did not stop")
        default:
            return .taken("a relay started by hand answers on the port")
        }
    }

    /// `proc_pidpath` of a pid, `nil` when it cannot be read.
    public static func processPath(_ pid: Int32) -> String? {
        var buffer = [CChar](repeating: 0, count: 4 * Int(MAXPATHLEN))
        let length = proc_pidpath(pid, &buffer, UInt32(buffer.count))
        guard length > 0 else { return nil }
        return String(decoding: buffer[..<Int(length)].map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }
}
