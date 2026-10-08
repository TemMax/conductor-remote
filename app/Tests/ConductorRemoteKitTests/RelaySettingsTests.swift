import Foundation
import Testing
@testable import ConductorRemoteKit

private func makeHome() throws -> RelayPaths {
    let home = FileManager.default.temporaryDirectory.appending(path: "relay-settings-\(UUID().uuidString)")
    let paths = RelayPaths(home: home)
    try FileManager.default.createDirectory(at: paths.stateDirectory, withIntermediateDirectories: true)
    return paths
}

private func writeSettings(_ json: String, to paths: RelayPaths) throws {
    try Data(json.utf8).write(to: paths.settingsFile)
}

@Test func portFromAString() throws {
    let paths = try makeHome()
    try writeSettings(#"{"RELAY_PORT": "9001"}"#, to: paths)
    #expect(RelaySettings.port(paths: paths) == 9001)
}

@Test func portFromANumber() throws {
    let paths = try makeHome()
    try writeSettings(#"{"RELAY_PORT": 9002}"#, to: paths)
    #expect(RelaySettings.port(paths: paths) == 9002)
}

@Test func portMissingGivesTheDefault() throws {
    let paths = try makeHome()
    #expect(RelaySettings.port(paths: paths) == RelayPaths.defaultPort)
    try writeSettings(#"{"OTHER": "1"}"#, to: paths)
    #expect(RelaySettings.port(paths: paths) == RelayPaths.defaultPort)
}

@Test func invalidPortGivesTheDefault() throws {
    let paths = try makeHome()
    let invalid = [
        #"{"RELAY_PORT": "0"}"#, #"{"RELAY_PORT": 0}"#, #"{"RELAY_PORT": 65536}"#,
        #"{"RELAY_PORT": "65536"}"#, #"{"RELAY_PORT": -5}"#, #"{"RELAY_PORT": "80a"}"#,
        #"{"RELAY_PORT": ""}"#, #"{"RELAY_PORT": true}"#, #"{"RELAY_PORT": 9001.5}"#,
        #"{"RELAY_PORT": null}"#, #"{"RELAY_PORT": [9001]}"#, "not json", "[]",
    ]
    for json in invalid {
        try writeSettings(json, to: paths)
        #expect(RelaySettings.port(paths: paths) == RelayPaths.defaultPort, "\(json)")
    }
}

@Test func portBoundsAreAccepted() throws {
    let paths = try makeHome()
    try writeSettings(#"{"RELAY_PORT": 1}"#, to: paths)
    #expect(RelaySettings.port(paths: paths) == 1)
    try writeSettings(#"{"RELAY_PORT": "65535"}"#, to: paths)
    #expect(RelaySettings.port(paths: paths) == 65535)
}

private let goodToken = "0123456789abcdef0123456789abcdef"

@Test func validTokenIsReturnedTrimmed() throws {
    let paths = try makeHome()
    try Data("\(goodToken)\n".utf8).write(to: paths.tokenFile)
    #expect(RelaySettings.token(paths: paths) == goodToken)
}

@Test func shortTokenIsNil() throws {
    let paths = try makeHome()
    try Data("0123456789abcdef".utf8).write(to: paths.tokenFile)
    #expect(RelaySettings.token(paths: paths) == nil)
}

@Test func upperCaseTokenIsNil() throws {
    let paths = try makeHome()
    try Data(goodToken.uppercased().utf8).write(to: paths.tokenFile)
    #expect(RelaySettings.token(paths: paths) == nil)
}

@Test func missingTokenIsNil() throws {
    let paths = try makeHome()
    #expect(RelaySettings.token(paths: paths) == nil)
}
