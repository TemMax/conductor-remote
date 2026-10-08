import Foundation
import Testing
@testable import ConductorRemoteKit

@Test func disabledAccessIsLocalOnlyEvenWithoutTailscale() {
    let report = TailnetReport(tailscale: false, mapped: false, enabled: false)
    #expect(PhoneLinkState(report: report, busy: false, hasLink: false) == .disabled)
}

@Test func disabledPreferenceWithActiveMappingIsAnError() {
    let report = TailnetReport(tailscale: true, mapped: true, enabled: false)
    #expect(PhoneLinkState(report: report, busy: false, hasLink: false) == .failed)
}

@Test func changingAccessHidesTheOldReadyLinkUntilConfirmed() {
    let report = TailnetReport(tailscale: true, mapped: true, url: "https://mac.example.ts.net/")
    #expect(PhoneLinkState(report: report, busy: true, hasLink: true) == .checking)
    #expect(PhoneLinkState(report: report, busy: false, hasLink: true)
        == .ready(address: "https://mac.example.ts.net/"))
}

@Test func settingsErrorsAreNotReportedAsMissingTailscaleOrDisabledAccess() {
    let report = TailnetReport(tailscale: false, mapped: false, error: "settings unreadable", enabled: nil)
    #expect(PhoneLinkState(report: report, busy: false, hasLink: false) == .failed)
    #expect(!report.canChangeAccess)
    let overridden = TailnetReport(tailscale: true, mapped: false, enabled: false, exposeSource: "environment")
    #expect(!overridden.canChangeAccess)
}

@Test func enabledAccessWithoutAMappingOffersSetupOnlyWithKnownSettings() {
    #expect(PhoneLinkState(report: TailnetReport(tailscale: true, mapped: false), busy: false, hasLink: false) == .notSetUp)
    #expect(PhoneLinkState(report: TailnetReport(tailscale: false, mapped: false), busy: false, hasLink: false) == .tailscaleMissing)
    #expect(PhoneLinkState(report: nil, busy: false, hasLink: false) == .checking)
    #expect(PhoneLinkState(report: TailnetReport(tailscale: true, mapped: false, enabled: nil), busy: false, hasLink: false) == .failed)
}
