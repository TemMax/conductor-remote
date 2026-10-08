import Foundation

/// Where the relay keeps its files, for one user's home directory.
public struct RelayPaths: Sendable, Equatable {
    /// The app's and the relay's bundle identifier; the Accessibility grant is tied to it.
    public static let bundleIdentifier = "com.temmax.conductor-remote"
    /// The port the relay listens on when `settings.json` names none.
    public static let defaultPort: UInt16 = 8790
    /// The `PATH` the relay gets: the one the LaunchAgent gave it.
    public static let relayPath = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

    public let home: URL

    public init(home: URL = FileManager.default.homeDirectoryForCurrentUser) {
        self.home = home
    }

    public var stateDirectory: URL { home.appending(path: "Library/Application Support/\(Self.bundleIdentifier)", directoryHint: .isDirectory) }
    public var tokenFile: URL { stateDirectory.appending(path: "token") }
    public var settingsFile: URL { stateDirectory.appending(path: "settings.json") }
    public var logDirectory: URL { home.appending(path: "Library/Logs/\(Self.bundleIdentifier)", directoryHint: .isDirectory) }
    public var relayLog: URL { logDirectory.appending(path: "relay.log") }
    public var relayErrorLog: URL { logDirectory.appending(path: "relay.err.log") }
    public var legacyLaunchAgent: URL { home.appending(path: "Library/LaunchAgents/\(Self.bundleIdentifier).plist") }

    /// The whole environment of every program the app runs from the relay's binary: `HOME`, the
    /// LaunchAgent's `PATH`, and `LANG`.
    public var relayEnvironment: [String: String] {
        ["HOME": home.path, "PATH": Self.relayPath, "LANG": "en_US.UTF-8"]
    }

    /// The relay inside an app bundle.
    public static func relayExecutable(inBundle bundle: URL) -> URL {
        bundle.appending(path: "Contents/MacOS/conductor-remote")
    }
}
