import AppKit
import ConductorRemoteKit
import SwiftUI

/// The first run: what has to be in place before the phone can reach Conductor.
struct OnboardingView: View {
    let model: AppModel
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Welcome to Conductor Remote").font(.title2.bold())
                Text("Conductor Remote lives in the menu bar and lets your phone work with Conductor on this Mac.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if model.oldServiceFound {
                ChecklistStep(mark: .attention, title: "Old background service",
                              detail: "An earlier version installed it. This app replaces it.",
                              action: RowAction("Replace") { model.replaceOldService() })
                    .disabled(model.checkingPort)
            }
            if let reason = model.portTakenReason {
                ChecklistStep(mark: .attention, title: "Relay", detail: "It cannot start: \(reason).",
                              action: RowAction("Try again") { model.tryAgain() })
                    .disabled(model.checkingPort)
            }
            accessibilityStep
            conductorStep
            tailscaleStep
            phoneStep
            if let error = model.lastError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button("Done") {
                    model.finishWelcome()
                    dismissWindow(id: "welcome")
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(24)
        .frame(width: 440)
        .appWindow()
        // Back from installing Tailscale: look again, so the step stays live.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            if model.phoneLinkState == .tailscaleMissing { model.refreshPhoneLink() }
        }
    }

    private var accessibilityStep: ChecklistStep {
        if model.accessibilityTrusted {
            ChecklistStep(mark: .done, title: "Accessibility", detail: "Granted.")
        } else {
            ChecklistStep(mark: .attention, title: "Accessibility",
                          detail: "Conductor Remote needs it to work with Conductor's window.",
                          action: RowAction("Grant…") { model.grantAccessibility() })
        }
    }

    private var conductorStep: ChecklistStep {
        if let status = model.status {
            status.conductor.running
                ? ChecklistStep(mark: .done, title: "Conductor", detail: "Running.")
                : ChecklistStep(mark: .attention, title: "Conductor", detail: "Not running. Open Conductor.")
        } else {
            ChecklistStep(mark: .waiting, title: "Conductor", detail: "Waiting for the relay.")
        }
    }

    private var tailscaleStep: ChecklistStep {
        switch model.phoneLinkState {
        case .ready(let address):
            ChecklistStep(mark: .done, title: "Tailscale", detail: address)
        case .checking:
            ChecklistStep(mark: .waiting, title: "Tailscale", detail: "Checking…")
        case .tailscaleMissing:
            ChecklistStep(mark: .attention, title: "Tailscale",
                          detail: "Missing. The phone reaches this Mac over your tailnet.",
                          action: RowAction("Get Tailscale") { model.getTailscale() })
        case .notSetUp:
            ChecklistStep(mark: .waiting, title: "Tailscale", detail: "The relay is not on your tailnet yet.",
                          action: RowAction("Set up") { model.setUpPhoneLink() })
        }
    }

    @ViewBuilder private var phoneStep: some View {
        if let link = model.phoneLink {
            ChecklistStep(mark: .waiting, title: "Phone",
                          detail: "Scan the code with your phone, open the link and add it to the Home Screen.")
            QRCodeView(text: link.absoluteString)
                .frame(width: 180, height: 180)
                .frame(maxWidth: .infinity)
        } else {
            ChecklistStep(mark: .waiting, title: "Phone", detail: "The code to scan appears here after the steps above.")
        }
    }
}

/// One line of the checklist: its state, what it is, and at most one button.
private struct ChecklistStep: View {
    enum Mark {
        case done, attention, waiting
    }

    let mark: Mark
    let title: String
    let detail: String
    var action: RowAction?

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            symbol.font(.title3).frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).fontWeight(.medium)
                Text(detail)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            if let action {
                Button(action.title, action: action.run)
            }
        }
    }

    @ViewBuilder private var symbol: some View {
        switch mark {
        case .done:
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .attention:
            Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
        case .waiting:
            Image(systemName: "circle").foregroundStyle(.secondary)
        }
    }
}
