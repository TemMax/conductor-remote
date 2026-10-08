import Foundation

/// The confirmed state shown by the menu, onboarding and phone-link window.
public enum PhoneLinkState: Equatable {
    case ready(address: String)
    case checking
    case tailscaleMissing
    case notSetUp
    case disabled
    case failed

    public init(report: TailnetReport?, busy: Bool, hasLink: Bool) {
        guard !busy, let report else { self = .checking; return }
        guard report.error == nil, let enabled = report.enabled else { self = .failed; return }
        if !enabled {
            self = report.mapped ? .failed : .disabled
        } else if hasLink, report.mapped, let address = report.url {
            self = .ready(address: address)
        } else {
            self = report.tailscale ? .notSetUp : .tailscaleMissing
        }
    }
}
