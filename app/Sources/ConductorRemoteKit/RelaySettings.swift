import Foundation

/// What the app reads of the relay's own files: the port and the token.
public enum RelaySettings {
    /// `RELAY_PORT` from `settings.json`, else `RelayPaths.defaultPort`.
    ///
    /// The value is a string of digits or a JSON integer in 1–65535; a boolean, a fractional
    /// number, an unreadable file or anything else gives the default, as it does for the relay.
    public static func port(paths: RelayPaths) -> UInt16 {
        guard let data = try? Data(contentsOf: paths.settingsFile),
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let value = root["RELAY_PORT"]
        else { return RelayPaths.defaultPort }
        guard let port = parsePort(value), port != 0 else { return RelayPaths.defaultPort }
        return port
    }

    /// The trimmed content of the token file when it is 32 lower-case hex characters, else `nil`.
    public static func token(paths: RelayPaths) -> String? {
        guard let text = try? String(contentsOf: paths.tokenFile, encoding: .utf8) else { return nil }
        let token = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let hex = Set("0123456789abcdef")
        guard token.utf8.count == 32, token.allSatisfy({ hex.contains($0) }) else { return nil }
        return token
    }

    private static func parsePort(_ value: Any) -> UInt16? {
        if let text = value as? String {
            guard !text.isEmpty, text.utf8.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }) else { return nil }
            return UInt16(text)
        }
        guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
        switch CFNumberGetType(number) {
        case .sInt8Type, .sInt16Type, .sInt32Type, .sInt64Type, .charType, .shortType, .intType,
             .longType, .longLongType, .cfIndexType, .nsIntegerType:
            return UInt16(exactly: number.int64Value)
        default:
            return nil
        }
    }
}
