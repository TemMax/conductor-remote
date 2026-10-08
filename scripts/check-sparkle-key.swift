// Checks that a Sparkle EdDSA private key belongs to the public key the app ships (SUPublicEDKey).
// usage: swift scripts/check-sparkle-key.swift <private-key-file> <expected-public-key-base64>
// Exit codes: 0 match, 1 mismatch, 2 unreadable input. The private key is never printed.
import CryptoKit
import Foundation

func fail(_ message: String, _ code: Int32) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(code)
}

let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    fail("usage: swift scripts/check-sparkle-key.swift <private-key-file> <expected-public-key-base64>", 2)
}
guard let text = try? String(contentsOfFile: arguments[1], encoding: .utf8) else {
    fail("cannot read the Sparkle private key file", 2)
}
let encoded = text.filter { !$0.isWhitespace }
guard let key = Data(base64Encoded: encoded) else {
    fail("the Sparkle private key file is not base64", 2)
}
let expected = arguments[2].trimmingCharacters(in: .whitespacesAndNewlines)

var matches = false
switch key.count {
case 32:
    // Sparkle 2 (`generate_keys -x`): the Ed25519 seed.
    guard let privateKey = try? Curve25519.Signing.PrivateKey(rawRepresentation: key) else {
        fail("the Sparkle private key is not a valid Ed25519 seed", 2)
    }
    matches = privateKey.publicKey.rawRepresentation.base64EncodedString() == expected
case 96:
    // Older format: a 64-byte expanded secret, then the 32-byte public key.
    let publicKey = key.subdata(in: 64..<96)
    matches = Data(base64Encoded: expected) == publicKey
default:
    fail("the Sparkle private key has an unexpected length (\(key.count) bytes; expected 32 or 96)", 2)
}

if matches {
    print("the Sparkle private key matches SUPublicEDKey")
} else {
    fail("the Sparkle private key does not match the app's SUPublicEDKey", 1)
}
