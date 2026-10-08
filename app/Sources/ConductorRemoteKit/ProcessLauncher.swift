import Foundation

/// A program to start.
public struct LaunchRequest: Sendable, Equatable {
    public var executable: URL
    public var arguments: [String]
    /// The whole environment of the child; nothing is inherited.
    public var environment: [String: String]
    /// Files the child's standard output and standard error are appended to; `nil` discards.
    public var standardOutput: URL?
    public var standardError: URL?
    public var currentDirectory: URL?

    public init(executable: URL, arguments: [String], environment: [String: String],
                standardOutput: URL? = nil, standardError: URL? = nil, currentDirectory: URL? = nil) {
        self.executable = executable
        self.arguments = arguments
        self.environment = environment
        self.standardOutput = standardOutput
        self.standardError = standardError
        self.currentDirectory = currentDirectory
    }
}

/// A started program.
public protocol RunningProcess: AnyObject, Sendable {
    var pid: Int32 { get }
    var isRunning: Bool { get }
    /// SIGTERM.
    func terminate()
    /// SIGKILL.
    func kill()
    /// Waits for the exit and returns the termination status.
    func waitForExit() async -> Int32
}

/// Starts programs; tests pass a fake.
public protocol ProcessLauncher: Sendable {
    /// Starts a long-running program.
    func launch(_ request: LaunchRequest) throws -> any RunningProcess
    /// Runs a program to its end and returns its exit status and standard output; the request's
    /// output files are ignored. Throws when it does not finish within `timeout`.
    func run(_ request: LaunchRequest, timeout: Duration) async throws -> (status: Int32, output: Data)
}
