import Foundation
import Testing
@testable import ConductorRemoteKit

private func shell(_ script: String, environment: [String: String] = [:], output: URL? = nil,
                   error: URL? = nil, directory: URL? = nil) -> LaunchRequest {
    LaunchRequest(executable: URL(filePath: "/bin/sh"), arguments: ["-c", script], environment: environment,
                  standardOutput: output, standardError: error, currentDirectory: directory)
}

private func sleeper(_ seconds: Int = 30) -> LaunchRequest {
    LaunchRequest(executable: URL(filePath: "/bin/sleep"), arguments: ["\(seconds)"], environment: [:])
}

private func scratchDirectory() throws -> URL {
    let url = FileManager.default.temporaryDirectory.appending(path: "crk-\(UUID().uuidString)", directoryHint: .isDirectory)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

private func permissions(_ url: URL) throws -> Int {
    try #require(FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions] as? Int)
}

@Test(.timeLimit(.minutes(1))) func runCapturesStandardOutputAndStatus() async throws {
    let result = try await SystemProcessLauncher().run(shell("printf hello; exit 3"), timeout: .seconds(10))
    #expect(result.status == 3)
    #expect(String(decoding: result.output, as: UTF8.self) == "hello")
}

@Test(.timeLimit(.minutes(1))) func runDiscardsStandardError() async throws {
    let result = try await SystemProcessLauncher().run(shell("echo out; echo err >&2"), timeout: .seconds(10))
    #expect(String(decoding: result.output, as: UTF8.self) == "out\n")
}

@Test(.timeLimit(.minutes(1))) func runKillsAndThrowsOnTimeout() async throws {
    let started = ContinuousClock.now
    await #expect(throws: SystemProcessLauncher.Error.timedOut) {
        _ = try await SystemProcessLauncher().run(sleeper(), timeout: .milliseconds(200))
    }
    #expect(ContinuousClock.now - started < .seconds(10))
}

@Test(.timeLimit(.minutes(1))) func launchAppendsToTheFilesAndCreatesThemPrivately() async throws {
    let root = try scratchDirectory()
    defer { try? FileManager.default.removeItem(at: root) }
    let logs = root.appending(path: "Logs", directoryHint: .isDirectory)
    let out = logs.appending(path: "out.log"), err = logs.appending(path: "err.log")
    let launcher = SystemProcessLauncher()
    for round in ["one", "two"] {
        let process = try launcher.launch(shell("echo \(round); echo e-\(round) >&2", output: out, error: err))
        #expect(await process.waitForExit() == 0)
    }
    #expect(try String(contentsOf: out, encoding: .utf8) == "one\ntwo\n")
    #expect(try String(contentsOf: err, encoding: .utf8) == "e-one\ne-two\n")
    #expect(try permissions(logs) == 0o700)
    #expect(try permissions(out) == 0o600)
    #expect(try permissions(err) == 0o600)
}

@Test(.timeLimit(.minutes(1))) func launchWithoutFilesDiscardsOutput() async throws {
    let process = try SystemProcessLauncher().launch(shell("echo lost; echo lost >&2"))
    #expect(await process.waitForExit() == 0)
}

@Test(.timeLimit(.minutes(1))) func launchGivesTheChildExactlyTheRequestedEnvironment() async throws {
    let root = try scratchDirectory()
    defer { try? FileManager.default.removeItem(at: root) }
    let out = root.appending(path: "env.txt")
    let script = #"printf '%s|%s|%s|%s' "$A" "$B" "${HOME-unset}" "${USER-unset}""#
    let process = try SystemProcessLauncher().launch(shell(script, environment: ["A": "1", "B": "2"], output: out))
    #expect(await process.waitForExit() == 0)
    #expect(try String(contentsOf: out, encoding: .utf8) == "1|2|unset|unset")
}

@Test(.timeLimit(.minutes(1))) func launchRunsInTheCurrentDirectory() async throws {
    let root = try scratchDirectory()
    defer { try? FileManager.default.removeItem(at: root) }
    let out = root.appending(path: "pwd.txt")
    let process = try SystemProcessLauncher().launch(shell("pwd -P", output: out, directory: root))
    #expect(await process.waitForExit() == 0)
    let printed = try String(contentsOf: out, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
    let resolved = try #require(realpath(root.path, nil))
    defer { free(resolved) }
    #expect(printed == String(cString: resolved))
}

@Test(.timeLimit(.minutes(1))) func waitForExitReturnsTheStatusAndAtOnceAfterTheEnd() async throws {
    let process = try SystemProcessLauncher().launch(shell("sleep 0.2; exit 7"))
    #expect(process.pid > 0)
    #expect(await process.waitForExit() == 7)
    #expect(process.isRunning == false)
    #expect(await process.waitForExit() == 7)
}

@Test(.timeLimit(.minutes(1))) func severalWaitersAllGetTheStatus() async throws {
    let process = try SystemProcessLauncher().launch(shell("sleep 0.2; exit 4"))
    async let first = process.waitForExit()
    async let second = process.waitForExit()
    #expect(await [first, second] == [4, 4])
}

@Test(.timeLimit(.minutes(1))) func terminateSendsSIGTERM() async throws {
    let process = try SystemProcessLauncher().launch(sleeper())
    #expect(process.isRunning)
    process.terminate()
    #expect(await process.waitForExit() == SIGTERM)
    #expect(process.isRunning == false)
}

@Test(.timeLimit(.minutes(1))) func killSendsSIGKILL() async throws {
    let process = try SystemProcessLauncher().launch(sleeper())
    process.kill()
    #expect(await process.waitForExit() == SIGKILL)
}

@Test(.timeLimit(.minutes(1))) func signalsAfterTheEndDoNothing() async throws {
    let process = try SystemProcessLauncher().launch(shell("exit 0"))
    #expect(await process.waitForExit() == 0)
    process.terminate()
    process.kill()
    #expect(await process.waitForExit() == 0)
}

@Test func launchOfAMissingProgramThrows() {
    let request = LaunchRequest(executable: URL(filePath: "/nonexistent/program"), arguments: [], environment: [:])
    #expect(throws: (any Error).self) { try SystemProcessLauncher().launch(request) }
}
