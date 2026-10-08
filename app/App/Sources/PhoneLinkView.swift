import AppKit
import ConductorRemoteKit
import SwiftUI

/// The phone link as a QR code, or what it still waits for.
struct PhoneLinkView: View {
    let model: AppModel

    var body: some View {
        VStack(spacing: 14) {
            if let link = model.phoneLink, case .ready(let address) = model.phoneLinkState {
                QRCodeView(text: link.absoluteString)
                    .frame(width: 220, height: 220)
                Text(address)
                    .font(.callout)
                    .multilineTextAlignment(.center)
                    .textSelection(.enabled)
                Button("Copy link") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(link.absoluteString, forType: .string)
                }
                Text("Open it on your phone and add it to the Home Screen.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            } else {
                missing
            }
            Button("Refresh") { model.refreshPhoneLink() }
                .disabled(model.tailnetBusy)
        }
        .padding(24)
        .frame(width: 320)
        .appWindow()
    }

    /// What is missing, and the button that helps.
    @ViewBuilder private var missing: some View {
        let (text, action) = missingStep
        Image(systemName: "iphone.slash")
            .font(.system(size: 36))
            .foregroundStyle(.secondary)
        Text(text).multilineTextAlignment(.center)
        if let error = model.lastError {
            Text(error)
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        if let action {
            Button(action.title, action: action.run).disabled(model.checkingPort || model.tailnetBusy)
        }
    }

    private var missingStep: (String, RowAction?) {
        if model.oldServiceFound {
            return ("The old background service is still installed, so the relay is not running.",
                    RowAction("Replace old service") { model.replaceOldService() })
        }
        if let reason = model.portTakenReason {
            return ("The relay cannot start: \(reason).", RowAction("Try again") { model.tryAgain() })
        }
        switch model.relayState {
        case .running:
            break
        case .starting, .restarting:
            return ("The relay is starting.", nil)
        case .stopped, .failed:
            return ("The relay is not running.", RowAction("Restart relay") { model.restartRelay() })
        }
        switch model.phoneLinkState {
        case .disabled:
            return ("Remote access is off. You can still use Remote on this Mac. Enable Access over Tailscale in Settings to connect from other devices.", nil)
        case .failed:
            return ("Remote access could not be confirmed.", RowAction("Retry") { model.setUpPhoneLink() })
        case .checking:
            return ("Looking for the relay on your tailnet…", nil)
        case .tailscaleMissing:
            return ("Tailscale is missing. The phone reaches this Mac over your tailnet.",
                    RowAction("Get Tailscale") { model.getTailscale() })
        case .notSetUp, .ready:
            return ("The relay is not on your tailnet yet.", RowAction("Set up") { model.setUpPhoneLink() })
        }
    }
}

/// A QR code on white, as large as its frame.
struct QRCodeView: View {
    let text: String

    var body: some View {
        if let image = QRCode.image(for: text) {
            Image(decorative: image, scale: 1)
                .interpolation(.none)
                .resizable()
                .scaledToFit()
                .padding(8)
                .background(Color.white, in: RoundedRectangle(cornerRadius: 6))
        }
    }
}
