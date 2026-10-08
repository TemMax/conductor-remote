import Foundation

public enum StatusError: Error, Equatable {
    /// Nothing answers on the port.
    case unreachable
    /// The relay answered with this status code.
    case http(Int)
    /// The answer is not a host status.
    case invalid
}

/// Asks the local relay for its status.
public struct StatusClient: Sendable {
    private let port: UInt16
    private let token: String
    private let session: URLSession

    public init(port: UInt16, token: String, session: URLSession = .shared) {
        self.port = port
        self.token = token
        self.session = session
    }

    /// `GET /api/host/status`.
    public func status() async throws -> HostStatus {
        var request = URLRequest(url: URL(string: "http://127.0.0.1:\(port)/api/host/status")!)
        request.httpMethod = "GET"
        request.timeoutInterval = 2
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")

        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch let error as URLError {
            switch error.code {
            case .cannotConnectToHost, .networkConnectionLost, .timedOut, .cannotFindHost:
                throw StatusError.unreachable
            default:
                throw error
            }
        }
        guard let http = response as? HTTPURLResponse else { throw StatusError.invalid }
        guard http.statusCode == 200 else { throw StatusError.http(http.statusCode) }
        do {
            return try JSONDecoder().decode(HostStatus.self, from: data)
        } catch {
            throw StatusError.invalid
        }
    }
}
