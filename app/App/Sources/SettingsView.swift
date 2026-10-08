import AppKit
import ConductorRemoteKit
import SwiftUI

/// Launch at login, remote access, updates, and what the app is.
struct SettingsView: View {
    let model: AppModel
    @ObservedObject var updater: UpdateController
    let launchAtLogin: LaunchAtLogin
    @Environment(\.openWindow) private var openWindow
    /// The text of the bundle's `Credits.html`; `nil` when the bundle has none.
    @State private var credits: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            general
            Divider()
            access
            Divider()
            updates
            Divider()
            about
        }
        .padding(20)
        .frame(width: 420)
        .appWindow()
        .task {
            credits = Self.readCredits()
            model.refreshPhoneLink()
        }
    }

    @ViewBuilder private var general: some View {
        Text("General").font(.headline)
        Toggle("Launch at login", isOn: Binding(
            get: { launchAtLogin.isEnabled },
            set: { launchAtLogin.setEnabled($0) }))
        if let error = launchAtLogin.lastError {
            caption(error)
        }
    }

    @ViewBuilder private var access: some View {
        Text("Remote access").font(.headline)
        Toggle("Access over Tailscale", isOn: Binding(
            get: { model.tailnetEnabled },
            set: { model.setTailnetAccess($0) }))
            .disabled(!model.canChangeTailnetAccess)
        caption("Connect from your phone or other devices on your Tailscale network. When off, Remote is available only on this Mac.")
        if model.tailnetBusy {
            caption("Checking access…")
        } else if model.tailnet?.exposeSource == "environment" {
            caption("Controlled by the EXPOSE environment variable.")
        }
        if let error = model.tailnet?.error {
            caption(error)
            Button("Retry") { model.setUpPhoneLink() }
                .disabled(model.tailnetBusy)
        }
    }

    @ViewBuilder private var updates: some View {
        Text("Updates").font(.headline)
        Toggle("Install updates automatically", isOn: $updater.installAutomatically)
        caption("Updates install while the relay is idle.")
        HStack {
            Button("Check for Updates…") { updater.checkForUpdates() }
                .disabled(!updater.canCheckForUpdates)
            Spacer()
            Text("Current version: \(Self.version)")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var about: some View {
        Text("About").font(.headline)
        Text("Conductor Remote \(Self.version) (\(Self.build))")
            .textSelection(.enabled)
        if let credits {
            ScrollView {
                Text(credits)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .frame(height: 140)
            .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 6))
        }
        HStack(spacing: 16) {
            Button("Open logs folder") {
                NSWorkspace.shared.open(RelayPaths().logDirectory)
            }
            Button("Show phone link") {
                WindowActivation.shared.open("phone", with: openWindow)
            }
        }
        .buttonStyle(.link)
    }

    private func caption(_ text: String) -> some View {
        Text(text)
            .font(.caption)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
    }

    private static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? ""
    }

    private static var build: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? ""
    }

    /// The credits as plain text, so they follow the window's colours.
    private static func readCredits() -> String? {
        guard let url = Bundle.main.url(forResource: "Credits", withExtension: "html"),
              let data = try? Data(contentsOf: url),
              let html = try? NSAttributedString(
                  data: data,
                  options: [.documentType: NSAttributedString.DocumentType.html,
                            .characterEncoding: String.Encoding.utf8.rawValue],
                  documentAttributes: nil)
        else { return nil }
        return html.string.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
