import CoreGraphics
import Testing
@testable import ConductorRemoteKit

@Test func qrCodeForALinkIsASquareImage() throws {
    let image = try #require(QRCode.image(for: "https://mac.example.ts.net:8443/#token=made-up-token"))
    #expect(image.width > 0)
    #expect(image.width == image.height)
}

@Test func qrCodeScalesWithoutChangingTheShape() throws {
    let small = try #require(QRCode.image(for: "https://mac.example.ts.net/", scale: 2))
    let large = try #require(QRCode.image(for: "https://mac.example.ts.net/", scale: 8))
    #expect(large.width == small.width * 4)
    #expect(large.width == large.height)
}
