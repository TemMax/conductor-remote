import AppKit
import ApplicationServices
import ConductorRemoteKit
import Observation

/// The app's state, and the kit objects behind it for the app's life.
@MainActor
@Observable
final class AppModel {
    /// The app's one model, set when the app creates it.
    static var shared: AppModel?

    private static let welcomeDoneKey = "welcomeDone"

    /// Not isolated, so the delegate can stop the relay without the main actor.
    nonisolated let supervisor: RelaySupervisor

    private(set) var relayState: RelayState = .stopped
    private(set) var status: HostStatus?
    private(set) var tailnet: TailnetReport?
    /// The tailnet URL with the token; `nil` until the relay is mapped and the token exists.
    private(set) var phoneLink: URL?
    /// The app's own Accessibility trust.
    private(set) var accessibilityTrusted: Bool
    /// Whether the old LaunchAgent is installed.
    private(set) var legacyInstalled: Bool
    /// Who holds the relay's port; `nil` until it has been looked at.
    private(set) var portOwner: PortOwner?
    private(set) var lastError: String?
    private(set) var tailnetBusy = false
    /// The port is being looked at, or the old service is being removed.
    private(set) var checkingPort = false
    private(set) var port: UInt16
    private(set) var welcomeDone: Bool
    private(set) var launched = false

    @ObservationIgnored private let paths: RelayPaths
    @ObservationIgnored private let relay: URL
    @ObservationIgnored private let legacy: LegacyService
    @ObservationIgnored private let tailnetCommand: TailnetCommand
    @ObservationIgnored private var token: String?
    @ObservationIgnored private var grantWatch = GrantWatch()
    @ObservationIgnored private var started = false
    @ObservationIgnored private var tailnetChecked = false

    init() {
        let paths = RelayPaths()
        let relay = RelayPaths.relayExecutable(inBundle: Bundle.main.bundleURL)
        let launcher = SystemProcessLauncher()
        let legacy = LegacyService(paths: paths, relay: relay, launcher: launcher)
        self.paths = paths
        self.relay = relay
        self.legacy = legacy
        supervisor = RelaySupervisor(relay: relay, paths: paths, parentPID: getpid(), launcher: launcher)
        tailnetCommand = TailnetCommand(relay: relay, paths: paths, launcher: launcher)
        port = RelaySettings.port(paths: paths)
        token = RelaySettings.token(paths: paths)
        legacyInstalled = legacy.isInstalled
        accessibilityTrusted = AXIsProcessTrusted()
        welcomeDone = UserDefaults.standard.bool(forKey: Self.welcomeDoneKey)
    }

    // MARK: - Derived state

    var isRelayRunning: Bool {
        if case .running = relayState { return true }
        return false
    }

    /// The old service stands in the relay's way: its LaunchAgent is installed, or its relay
    /// answers on the port.
    var oldServiceFound: Bool {
        legacyInstalled || portOwner == .legacyService
    }

    /// Why the port cannot be used, when something else holds it.
    var portTakenReason: String? {
        if case .taken(let reason) = portOwner { return reason }
        return nil
    }

    var phoneLinkState: PhoneLinkState {
        PhoneLinkState(report: tailnet, busy: tailnetBusy, hasLink: phoneLink != nil)
    }

    var tailnetEnabled: Bool { tailnet?.enabled == true }
    var canChangeTailnetAccess: Bool { !tailnetBusy && tailnet?.canChangeAccess == true }

    /// The relay's version once it has answered, else the app's.
    var version: String {
        status?.version
            ?? Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? ""
    }

    /// The welcome window is due: the first launch, or the old service is in the way.
    var opensWelcome: Bool {
        launched && (!welcomeDone || oldServiceFound)
    }

    // MARK: - Launch

    /// Starts watching the relay, the status and the Accessibility grant, and starts the relay
    /// when the port allows it.
    func start() {
        guard !started else { return }
        started = true
        let states = supervisor.states
        Task {
            for await state in states { self.relayStateChanged(state) }
        }
        Task { await self.watchAccessibility() }
        Task { await self.pollStatus() }
        Task { await self.checkAndStart() }
    }

    /// The delegate calls this once the windows SwiftUI opened at launch are out of the way.
    func launchFinished() {
        launched = true
    }

    /// "Done" in the welcome window.
    func finishWelcome() {
        welcomeDone = true
        UserDefaults.standard.set(true, forKey: Self.welcomeDoneKey)
    }

    // MARK: - Actions

    /// "Try again" after the port was found taken.
    func tryAgain() {
        Task { await checkAndStart() }
    }

    /// "Replace old service": removes it, then looks at the port and starts the relay.
    func replaceOldService() {
        Task {
            guard !checkingPort else { return }
            checkingPort = true
            defer { checkingPort = false }
            lastError = nil
            do {
                try await legacy.remove()
            } catch LegacyServiceError.failed(let status) {
                lastError = "The old service could not be removed (exit \(status))."
                legacyInstalled = legacy.isInstalled
                return
            } catch {
                lastError = "The old service could not be removed: \(error.localizedDescription)"
                legacyInstalled = legacy.isInstalled
                return
            }
            legacyInstalled = legacy.isInstalled
            await startIfPortAllows()
        }
    }

    /// "Grant Accessibility…": the system's prompt, then the Accessibility list in System Settings.
    func grantAccessibility() {
        // The literal key: the `kAXTrustedCheckOptionPrompt` global is not usable in Swift 6 mode.
        _ = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt" as CFString: true] as CFDictionary)
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
            NSWorkspace.shared.open(url)
        }
    }

    /// "Restart relay". A relay that was never started is started the way it is at launch, so
    /// the port is looked at first.
    func restartRelay() {
        Task {
            if await supervisor.state == .stopped {
                await checkAndStart()
            } else {
                await supervisor.restart()
            }
        }
    }

    /// "Set up": retries the current preference, never overrides EXPOSE=off.
    func setUpPhoneLink() {
        Task { await ensureTailnet() }
    }

    /// Save the choice and reconcile only this relay's Tailscale mapping.
    func setTailnetAccess(_ enabled: Bool) {
        Task {
            guard canChangeTailnetAccess else { return }
            tailnetBusy = true
            defer { tailnetBusy = false }
            apply(await tailnetCommand.run(enabled ? .enable : .disable))
        }
    }

    /// "Refresh" in the phone view.
    func refreshPhoneLink() {
        Task {
            guard !tailnetBusy else { return }
            tailnetBusy = true
            defer { tailnetBusy = false }
            apply(await tailnetCommand.run(.status))
        }
    }

    /// "Get Tailscale".
    func getTailscale() {
        if let url = URL(string: "https://tailscale.com/download/mac") {
            NSWorkspace.shared.open(url)
        }
    }

    // MARK: - The relay's port

    /// At launch: with the old LaunchAgent installed the relay is not started; otherwise the
    /// port decides.
    private func checkAndStart() async {
        guard !checkingPort else { return }
        checkingPort = true
        defer { checkingPort = false }
        lastError = nil
        portOwner = nil
        port = RelaySettings.port(paths: paths)
        readToken()
        legacyInstalled = legacy.isInstalled
        guard !legacyInstalled else { return }
        await startIfPortAllows()
    }

    private func startIfPortAllows() async {
        // The guard stops a relay this app left behind, and would take a running one for it.
        guard await supervisor.state == .stopped else { return }
        let port = port
        let owner: PortOwner
        if let token {
            let client = StatusClient(port: port, token: token)
            let orphanGuard = OrphanGuard(
                relay: relay,
                status: { try await client.status() },
                portFree: { await Self.portIsFree(port) })
            owner = await orphanGuard.check()
        } else if await Self.portIsFree(port) {
            // A first run: no token yet, so no relay of ours can answer.
            owner = .free
        } else {
            owner = .taken("port \(port) is in use by another program")
        }
        portOwner = owner
        switch owner {
        case .free, .stoppedOrphan:
            await supervisor.start()
        case .legacyService, .taken:
            break
        }
    }

    /// Whether nothing accepts a TCP connection on `127.0.0.1:<port>`.
    private nonisolated static func portIsFree(_ port: UInt16) async -> Bool {
        let descriptor = socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else { return false }
        defer { close(descriptor) }
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        address.sin_addr = in_addr(s_addr: inet_addr("127.0.0.1"))
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        return result != 0
    }

    // MARK: - Watching

    private func relayStateChanged(_ state: RelayState) {
        relayState = state
        if !isRelayRunning, status != nil { status = nil }
    }

    /// Every 3 s while the relay runs.
    private func pollStatus() async {
        while !Task.isCancelled {
            if isRelayRunning { await refreshStatus() }
            try? await Task.sleep(for: .seconds(3))
        }
    }

    private func refreshStatus() async {
        // A first run has no token until the relay has written it.
        if token == nil { readToken() }
        guard let token else { return }
        let answer: HostStatus
        do {
            answer = try await StatusClient(port: port, token: token).status()
        } catch {
            if status != nil { status = nil }
            return
        }
        guard isRelayRunning else { return }
        if status != answer { status = answer }
        if grantWatch.observe(relay: answer, appTrusted: accessibilityTrusted) {
            await supervisor.restart()
        }
        // Once per launch, after the first good status.
        if !tailnetChecked {
            Task { await self.ensureTailnet() }
        }
    }

    /// Every 2 s: the grant reaches the relay only when it starts.
    private func watchAccessibility() async {
        while !Task.isCancelled {
            let trusted = AXIsProcessTrusted()
            if trusted != accessibilityTrusted { accessibilityTrusted = trusted }
            // A relay the port keeps stopped is not started by the grant.
            if grantWatch.observe(appTrusted: trusted), await supervisor.state != .stopped {
                await supervisor.restart()
            }
            try? await Task.sleep(for: .seconds(2))
        }
    }

    // MARK: - Phone link

    /// Respect EXPOSE at launch and remove a stale mapping when access is disabled.
    private func ensureTailnet() async {
        guard !tailnetBusy else { return }
        tailnetChecked = true
        tailnetBusy = true
        defer { tailnetBusy = false }
        apply(await tailnetCommand.reconcile())
    }

    private func apply(_ report: TailnetReport) {
        tailnet = report
        lastError = report.error
        if token == nil { readToken() }
        updatePhoneLink()
    }

    private func readToken() {
        token = RelaySettings.token(paths: paths)
        updatePhoneLink()
    }

    private func updatePhoneLink() {
        var link: URL?
        if let tailnet, let token {
            link = PhoneLink.url(tailnet: tailnet, token: token)
        }
        if link != phoneLink { phoneLink = link }
    }
}
