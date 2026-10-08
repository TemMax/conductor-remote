import AppKit
import ConductorRemoteKit
import SwiftUI

/// A button a row or a step offers.
struct RowAction {
    let title: String
    let run: () -> Void

    init(_ title: String, run: @escaping () -> Void) {
        self.title = title
        self.run = run
    }
}

/// The menu-bar panel: the state of each part, and one thing to do about it.
struct MenuPanel: View {
    let model: AppModel
    @ObservedObject var updater: UpdateController
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text("Conductor Remote").font(.headline)
                Spacer()
                Text(model.version).font(.caption).foregroundStyle(.secondary)
            }
            Divider()
            relayRow
            conductorRow
            accessibilityRow
            phoneLinkRow
            if model.oldServiceFound {
                StatusRow(color: .orange, title: "Old background service found",
                          action: RowAction("Replace") { model.replaceOldService() })
                    .disabled(model.checkingPort)
            }
            if let reason = model.portTakenReason {
                StatusRow(color: .red, title: "The relay cannot start", detail: reason,
                          action: RowAction("Try again") { model.tryAgain() })
                    .disabled(model.checkingPort)
            }
            if let error = model.lastError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Divider()
            VStack(spacing: 2) {
                MenuButton(title: "Show phone link…") {
                    WindowActivation.shared.open("phone", with: openWindow)
                }
                MenuButton(title: "Settings…") {
                    WindowActivation.shared.open("settings", with: openWindow)
                }
                MenuButton(title: "Check for Updates…") {
                    updater.checkForUpdates()
                }
                .disabled(!updater.canCheckForUpdates)
                MenuButton(title: "Quit Conductor Remote") {
                    NSApp.terminate(nil)
                }
            }
        }
        .padding(14)
        .frame(width: 320)
    }

    private var relayRow: some View {
        let restart = RowAction("Restart") { model.restartRelay() }
        // A relay the old service or another program keeps stopped has its own row and button.
        let blocked = model.oldServiceFound || model.portTakenReason != nil || model.checkingPort
        switch model.relayState {
        case .running(let pid):
            return StatusRow(color: .green, title: "Relay", detail: "Running · pid \(pid)", action: restart)
        case .starting:
            return StatusRow(color: .yellow, title: "Relay", detail: "Starting")
        case .restarting(let delay, _):
            return StatusRow(color: .yellow, title: "Relay",
                             detail: "Restarting in \(delay.components.seconds) s", action: restart)
        case .stopped:
            return StatusRow(color: .gray, title: "Relay", detail: "Stopped", action: blocked ? nil : restart)
        case .failed:
            return StatusRow(color: .red, title: "Relay", detail: "Failed", action: restart)
        }
    }

    private var conductorRow: some View {
        let running = model.status?.conductor.running == true
        return StatusRow(color: running ? .green : .gray, title: "Conductor",
                         detail: running ? "Running" : "Not running")
    }

    private var accessibilityRow: some View {
        if model.accessibilityTrusted {
            StatusRow(color: .green, title: "Accessibility", detail: "Granted")
        } else {
            StatusRow(color: .red, title: "Accessibility", detail: "Not granted",
                      action: RowAction("Grant…") { model.grantAccessibility() })
        }
    }

    private var phoneLinkRow: some View {
        switch model.phoneLinkState {
        case .ready(let address):
            StatusRow(color: .green, title: "Phone link", detail: address)
        case .checking:
            StatusRow(color: .yellow, title: "Phone link", detail: "Checking…")
        case .disabled:
            StatusRow(color: .gray, title: "Phone link", detail: "Off · local only")
        case .failed:
            StatusRow(color: .red, title: "Phone link", detail: "Access setup failed",
                      action: RowAction("Retry") { model.setUpPhoneLink() })
        case .tailscaleMissing:
            StatusRow(color: .red, title: "Phone link", detail: "Tailscale missing",
                      action: RowAction("Get Tailscale") { model.getTailscale() })
        case .notSetUp:
            StatusRow(color: .gray, title: "Phone link", detail: "Not set up",
                      action: RowAction("Set up") { model.setUpPhoneLink() })
        }
    }
}

/// A coloured dot, a name with its state, and at most one button.
private struct StatusRow: View {
    let color: Color
    let title: String
    var detail: String?
    var action: RowAction?

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(color).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                if let detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .truncationMode(.middle)
                }
            }
            Spacer(minLength: 8)
            if let action {
                Button(action.title, action: action.run).controlSize(.small)
            }
        }
    }
}

private struct MenuButton: View {
    let title: String
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Text(title)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(hovering ? Color.primary.opacity(0.1) : Color.clear, in: RoundedRectangle(cornerRadius: 5))
        .onHover { hovering = $0 }
    }
}
