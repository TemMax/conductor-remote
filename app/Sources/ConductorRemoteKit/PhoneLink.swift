import Foundation

/// The address the phone opens: the relay's tailnet URL with the token in the fragment.
public enum PhoneLink {
    /// `<report.url>#token=<token>` only when enabled access and its mapping are confirmed.
    public static func url(tailnet: TailnetReport, token: String) -> URL? {
        guard tailnet.enabled == true, tailnet.error == nil, tailnet.mapped,
              let base = tailnet.url else { return nil }
        return URL(string: "\(base)#token=\(token)")
    }
}
