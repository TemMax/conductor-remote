import Foundation
import Testing
@testable import ConductorRemoteKit

/// Answers every request of a session built by `StubURLProtocol.session()` through `handler`.
final class StubURLProtocol: URLProtocol, @unchecked Sendable {
    typealias Handler = @Sendable (URLRequest) throws -> (HTTPURLResponse, Data)
    private static let box = NSLock()
    nonisolated(unsafe) private static var handler: Handler?

    static func set(_ handler: Handler?) {
        box.lock()
        defer { box.unlock() }
        self.handler = handler
    }

    static func session() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        return URLSession(configuration: configuration)
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        Self.box.lock()
        let handler = Self.handler
        Self.box.unlock()
        guard let handler else {
            client?.urlProtocol(self, didFailWithError: URLError(.unknown))
            return
        }
        do {
            let (response, data) = try handler(request)
            client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
        }
    }

    override func stopLoading() {}
}

private let statusJSON = """
{
  "version": "0.1.0", "pid": 4242, "startedAt": 1759824000000, "supervisor": "app", "port": 8790,
  "conductor": { "running": true }, "accessibility": { "trusted": true },
  "screenLocked": null, "activity": { "working": 2, "idleMs": null }
}
"""

private func respond(_ request: URLRequest, _ code: Int, _ body: String) -> (HTTPURLResponse, Data) {
    (HTTPURLResponse(url: request.url!, statusCode: code, httpVersion: nil, headerFields: nil)!,
     Data(body.utf8))
}

@Suite(.serialized) struct StatusClientTests {
    private func client(port: UInt16 = 8790) -> StatusClient {
        StatusClient(port: port, token: "tok", session: StubURLProtocol.session())
    }

    @Test func decodesTheStatus() async throws {
        StubURLProtocol.set { respond($0, 200, statusJSON) }
        let status = try await client().status()
        #expect(status.pid == 4242)
        #expect(status.supervisor == "app")
        #expect(status.activity == .init(working: 2, idleMs: nil))
    }

    @Test func sendsTheBearerTokenToTheRightURL() async throws {
        let seen = Seen()
        StubURLProtocol.set { request in
            seen.set(request)
            return respond(request, 200, statusJSON)
        }
        _ = try await client(port: 9123).status()
        let request = try #require(seen.get())
        #expect(request.url?.absoluteString == "http://127.0.0.1:9123/api/host/status")
        #expect(request.httpMethod == "GET")
        #expect(request.value(forHTTPHeaderField: "Authorization") == "Bearer tok")
        #expect(request.timeoutInterval == 2)
    }

    @Test func unauthorizedThrowsHttp401() async {
        StubURLProtocol.set { respond($0, 401, "") }
        await #expect(throws: StatusError.http(401)) { try await client().status() }
    }

    @Test func unreachableCodesThrowUnreachable() async {
        for code in [URLError.Code.cannotConnectToHost, .networkConnectionLost, .timedOut, .cannotFindHost] {
            StubURLProtocol.set { _ in throw URLError(code) }
            await #expect(throws: StatusError.unreachable) { try await client().status() }
        }
    }

    @Test func undecodableBodyThrowsInvalid() async {
        StubURLProtocol.set { respond($0, 200, "<html></html>") }
        await #expect(throws: StatusError.invalid) { try await client().status() }
    }
}

private final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var request: URLRequest?
    func set(_ value: URLRequest) { lock.withLock { request = value } }
    func get() -> URLRequest? { lock.withLock { request } }
}
