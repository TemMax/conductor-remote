import Foundation
import Testing
@testable import ConductorRemoteKit

@Test func phoneLinkPutsTheTokenInTheFragment() {
    let report = TailnetReport(
        tailscale: true, host: "mac.example.ts.net", httpsPort: 443, mapped: true,
        url: "https://mac.example.ts.net/")
    #expect(PhoneLink.url(tailnet: report, token: "made-up-token")?.absoluteString
        == "https://mac.example.ts.net/#token=made-up-token")
}

@Test func phoneLinkKeepsThePort() {
    let report = TailnetReport(
        tailscale: true, host: "mac.example.ts.net", httpsPort: 8443, mapped: true,
        url: "https://mac.example.ts.net:8443/")
    let url = PhoneLink.url(tailnet: report, token: "made-up-token")
    #expect(url?.absoluteString == "https://mac.example.ts.net:8443/#token=made-up-token")
    #expect(url?.port == 8443)
    #expect(url?.fragment == "token=made-up-token")
}

@Test func phoneLinkNeedsAMapping() {
    let unmapped = TailnetReport(tailscale: true, host: "mac.example.ts.net", mapped: false)
    #expect(PhoneLink.url(tailnet: unmapped, token: "made-up-token") == nil)
    let noURL = TailnetReport(tailscale: true, mapped: true, url: nil)
    #expect(PhoneLink.url(tailnet: noURL, token: "made-up-token") == nil)
    let unmappedWithURL = TailnetReport(tailscale: true, mapped: false, url: "https://mac.example.ts.net/")
    #expect(PhoneLink.url(tailnet: unmappedWithURL, token: "made-up-token") == nil)
}
