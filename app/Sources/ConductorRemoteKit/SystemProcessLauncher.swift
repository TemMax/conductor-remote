import Foundation

/// Starts programs with Foundation's `Process`.
public final class SystemProcessLauncher: ProcessLauncher {
    public enum Error: Swift.Error, Equatable {
        /// The program was still running when the timeout passed; it has been killed.
        case timedOut
    }

    public init() {}

    public func launch(_ request: LaunchRequest) throws -> any RunningProcess {
        var opened: [FileHandle] = []
        defer { for handle in opened { try? handle.close() } }
        let output = try Self.openForAppending(request.standardOutput)
        opened.append(output)
        let error = try Self.openForAppending(request.standardError)
        opened.append(error)

        let process = Self.makeProcess(request)
        process.standardOutput = output
        process.standardError = error
        let running = SystemProcess(process)
        try running.start()
        return running
    }

    public func run(_ request: LaunchRequest, timeout: Duration) async throws -> (status: Int32, output: Data) {
        let pipe = Pipe()
        let process = Self.makeProcess(request)
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        let running = SystemProcess(process)
        do {
            try running.start()
        } catch {
            try? pipe.fileHandleForWriting.close()
            try? pipe.fileHandleForReading.close()
            throw error
        }
        // The child holds the write end now; ours must go for the read to see the end of the output.
        try? pipe.fileHandleForWriting.close()
        let reader = Task { await Self.readToEnd(pipe.fileHandleForReading) }

        let timedOut = Flag()
        let timer = Task {
            try? await Task.sleep(for: timeout)
            guard !Task.isCancelled, running.isRunning else { return }
            timedOut.set()
            running.kill()
        }
        let status = await withTaskCancellationHandler {
            await running.waitForExit()
        } onCancel: {
            running.kill()
        }
        timer.cancel()
        // A grandchild may still hold the pipe; after a timeout the output is not wanted.
        if timedOut.isSet { throw Error.timedOut }
        try Task.checkCancellation()
        return (status, await reader.value)
    }

    private static func makeProcess(_ request: LaunchRequest) -> Process {
        let process = Process()
        process.executableURL = request.executable
        process.arguments = request.arguments
        process.environment = request.environment
        process.currentDirectoryURL = request.currentDirectory
        return process
    }

    /// The file opened for appending (mode 0600, in a 0700 directory when it has to be created), or
    /// `/dev/null` when `url` is `nil`.
    private static func openForAppending(_ url: URL?) throws -> FileHandle {
        var path = "/dev/null"
        if let url {
            try FileManager.default.createDirectory(
                at: url.deletingLastPathComponent(), withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700])
            path = url.path
        }
        let descriptor = open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0o600)
        guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        return FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
    }

    private static func readToEnd(_ handle: FileHandle) async -> Data {
        await withCheckedContinuation { continuation in
            DispatchQueue.global().async {
                let data = handle.readDataToEndOfFile()
                try? handle.close()
                continuation.resume(returning: data)
            }
        }
    }
}

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func set() { lock.withLock { value = true } }
    var isSet: Bool { lock.withLock { value } }
}

/// A `Process` that remembers how it ended.
private final class SystemProcess: RunningProcess, @unchecked Sendable {
    private let process: Process
    private let lock = NSLock()
    private var status: Int32?
    private var waiters: [CheckedContinuation<Int32, Never>] = []

    init(_ process: Process) {
        self.process = process
        process.terminationHandler = { [weak self] process in
            self?.finished(process.terminationStatus)
        }
    }

    func start() throws {
        try process.run()
    }

    var pid: Int32 { process.processIdentifier }

    var isRunning: Bool { lock.withLock { status == nil } }

    func terminate() {
        lock.withLock {
            guard status == nil, process.isRunning else { return }
            process.terminate()
        }
    }

    func kill() {
        lock.withLock {
            guard status == nil, process.isRunning else { return }
            _ = Darwin.kill(process.processIdentifier, SIGKILL)
        }
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

    private func finished(_ terminationStatus: Int32) {
        let pending: [CheckedContinuation<Int32, Never>] = lock.withLock {
            status = terminationStatus
            defer { waiters = [] }
            return waiters
        }
        for continuation in pending { continuation.resume(returning: terminationStatus) }
    }
}
