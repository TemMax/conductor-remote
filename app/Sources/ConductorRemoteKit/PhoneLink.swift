import Foundation

/// The address the phone opens: the relay's tailnet URL with the token in the fragment.
public enum PhoneLink {
    /// `<report.url>#token=<token>` when the report is mapped and carries a URL, else `nil`.
    public static func url(tailnet: TailnetReport, token: String) -> URL? {
        guard tailnet.mapped, let base = tailnet.url else { return nil }
        return URL(string: "\(base)#token=\(token)")
    }
}
