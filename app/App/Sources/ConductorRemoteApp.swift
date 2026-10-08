import AppKit
import ConductorRemoteKit
import Sparkle
import SwiftUI

@main
struct ConductorRemoteApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model: AppModel
    @StateObject private var updater: UpdateController
    @State private var launchAtLogin: LaunchAtLogin

    init() {
        let model = AppModel()
        AppModel.shared = model
        _model = State(initialValue: model)
        // Made here and not by the wrapper, so Sparkle starts with the app and not with a view.
        let updater = UpdateController(status: { (model.isRelayRunning, model.status) })
        _updater = StateObject(wrappedValue: updater)
        let launchAtLogin = LaunchAtLogin()
        _launchAtLogin = State(initialValue: launchAtLogin)
        model.start()
        launchAtLogin.start()
    }

    var body: some Scene {
        MenuBarExtra {
            MenuPanel(model: model, updater: updater)
        } label: {
            MenuBarLabel(model: model)
        }
        .menuBarExtraStyle(.window)

        Window("Phone Link", id: "phone") {
            PhoneLinkView(model: model)
        }
        .windowResizability(.contentSize)
        .closedAtLaunch()

        Window("Welcome", id: "welcome") {
            OnboardingView(model: model)
        }
        .windowResizability(.contentSize)
        .closedAtLaunch()

        Window("Settings", id: "settings") {
            SettingsView(model: model, updater: updater, launchAtLogin: launchAtLogin)
        }
        .windowResizability(.contentSize)
        .closedAtLaunch()
    }
}

/// The menu-bar icon. It is rendered at launch, so it also opens the welcome window when that
/// is due.
private struct MenuBarLabel: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let needsAttention = !model.isRelayRunning || !model.accessibilityTrusted
        Image(systemName: needsAttention ? "iphone.slash" : "iphone.radiowaves.left.and.right")
            .accessibilityLabel("Conductor Remote")
            .task(id: model.opensWelcome) {
                if model.opensWelcome {
                    WindowActivation.shared.open("welcome", with: openWindow)
                }
            }
    }
}
