import Foundation

/// `GET /api/host/status`.
public struct HostStatus: Codable, Sendable, Equatable {
    public struct Conductor: Codable, Sendable, Equatable {
        public var running: Bool

        public init(running: Bool) {
            self.running = running
        }
    }
    public struct Accessibility: Codable, Sendable, Equatable {
        public var trusted: Bool

        public init(trusted: Bool) {
            self.trusted = trusted
        }
    }
    public struct Activity: Codable, Sendable, Equatable {
        public var working: Int
        public var idleMs: Int64?

        public init(working: Int, idleMs: Int64? = nil) {
            self.working = working
            self.idleMs = idleMs
        }
    }
    public var version: String
    public var pid: Int32
    public var startedAt: Int64
    /// "app", "launchd" or "none".
    public var supervisor: String
    public var port: UInt16
    public var conductor: Conductor
    public var accessibility: Accessibility
    public var screenLocked: Bool?
    public var activity: Activity

    public init(version: String, pid: Int32, startedAt: Int64, supervisor: String, port: UInt16,
                conductor: Conductor, accessibility: Accessibility, screenLocked: Bool? = nil,
                activity: Activity) {
        self.version = version
        self.pid = pid
        self.startedAt = startedAt
        self.supervisor = supervisor
        self.port = port
        self.conductor = conductor
        self.accessibility = accessibility
        self.screenLocked = screenLocked
        self.activity = activity
    }
}

/// `conductor-remote tailnet … --json`.
public struct TailnetReport: Codable, Sendable, Equatable {
    public var enabled: Bool?
    public var exposeSource: String?
    public var tailscale: Bool
    public var host: String?
    public var httpsPort: UInt16?
    public var mapped: Bool
    public var url: String?
    public var error: String?

    /// Unknown settings and an environment override cannot be edited through the app.
    public var canChangeAccess: Bool { enabled != nil && exposeSource != "environment" }

    public init(tailscale: Bool, host: String? = nil, httpsPort: UInt16? = nil, mapped: Bool,
                url: String? = nil, error: String? = nil,
                enabled: Bool? = true, exposeSource: String? = nil) {
        self.enabled = enabled
        self.exposeSource = exposeSource
        self.tailscale = tailscale
        self.host = host
        self.httpsPort = httpsPort
        self.mapped = mapped
        self.url = url
        self.error = error
    }
}
