import AppKit
import GravitonKit
import SwiftUI

struct SettingsScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(SharedHosts.self) private var hosts
    @Environment(Dialogs.self) private var dialogs
    @Environment(UpdateChecker.self) private var updates
    @AppStorage("showsMenuBarExtra") private var showsMenuBarExtra = true
    @AppStorage("menuBarShowsCount") private var menuBarShowsCount = true
    @State private var showLogin = false
    @State private var busyRegistry: String?
    @State private var statusBanner: (tone: GStatusLine.Tone, text: String)?
    @State private var copiedDockerEnv = false
    @State private var powerStatus = PowerStatus()
    @State private var offloadPolicy = OffloadPolicy()
    @State private var activeTunnels: [ActiveTunnel] = []
    @State private var savingOffload = false

    private var hostLabel: String { store.liveName ?? "This Mac" }

    var body: some View {
        VStack(spacing: 0) {
            ScreenHeader("Settings", subtitle: "Registry credentials & preferences on \(hostLabel)") {
                Button {
                    Task { await store.loadRegistryLogins() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.gSecondary)
                .help("Reload saved registry logins from \(hostLabel)")

                Button {
                    showLogin = true
                } label: {
                    Label("Sign In…", systemImage: "plus")
                }
                .buttonStyle(.gPrimary)
                .disabled(!store.canManage)
                .help(store.canManage ? "Sign in to an OCI registry and save its secret in Keychain" : "Connect to gravitond to sign in")
            }

            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    if let banner = statusBanner {
                        GStatusLine(banner.tone, banner.text)
                    }

                    loginsCard
                    offloadCard
                    updatesCard
                    generalCard
                    aboutCard
                }
                .frame(maxWidth: 780, alignment: .leading)
                .padding(.horizontal, 28)
                .padding(.vertical, 16)
            }
        }
        .background(G.bg)
        .task(id: "\(hostLabel)-\(store.connection == .live)") {
            await store.loadRegistryLogins()
            await loadOffloadAndTunnels()
        }
        .sheet(isPresented: $showLogin) {
            RegistryLoginSheet { info in
                statusBanner = (.success, "Signed in to \(info.registry) as \(info.username).")
            }
        }
    }

    private var loginsCard: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 8) {
                GCaption("OCI Registry Logins")
                if !store.registryLogins.isEmpty {
                    GTagPill("\(store.registryLogins.count)", color: G.indigo)
                }
                Spacer()
            }
            Text("Usernames are stored in registries.json (mode 0600) on \(hostLabel); passwords and tokens live in \(store.isLocal ? "this Mac's" : "\(hostLabel)'s") login Keychain under service dev.graviton.registry.")
                .font(.system(size: 12.5))
                .foregroundStyle(G.textMuted)

            if store.registryLogins.isEmpty {
                HStack(spacing: 12) {
                    Image(systemName: "key")
                        .font(.system(size: 16))
                        .foregroundStyle(G.textTertiary)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("No saved registry logins")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(G.textPrimary)
                        Text("Sign in to push images or pull private repositories.")
                            .font(.system(size: 12))
                            .foregroundStyle(G.textMuted)
                    }
                    Spacer()
                    Button("Sign In…") { showLogin = true }
                        .buttonStyle(.g(.secondary, size: .small))
                        .disabled(!store.canManage)
                }
                .padding(14)
                .background(G.bg, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            } else {
                VStack(spacing: 6) {
                    ForEach(store.registryLogins) { login in
                        HStack(spacing: 12) {
                            Image(systemName: "lock.shield")
                                .font(.system(size: 14))
                                .foregroundStyle(G.indigo)
                                .frame(width: 24)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(login.registry)
                                    .font(G.mono(13, weight: .medium))
                                    .foregroundStyle(G.textPrimary)
                                Text(login.username)
                                    .font(G.mono(11.5))
                                    .foregroundStyle(G.textMuted)
                            }
                            Spacer()
                            Button {
                                confirmLogout(login)
                            } label: {
                                if busyRegistry == login.registry {
                                    GSpinner(size: 11)
                                } else {
                                    Text("Sign Out")
                                }
                            }
                            .buttonStyle(.g(.danger, size: .small))
                            .disabled(!store.canManage || busyRegistry != nil)
                        }
                        .padding(.horizontal, 14)
                        .padding(.vertical, 11)
                        .background(G.bg, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    }
                }
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }

    private var offloadCard: some View {
        let battLabel = powerStatus.batteryPercent.map { "\(powerStatus.source) · \($0)%" } ?? powerStatus.source
        return VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 8) {
                GCaption("Battery-Aware Burst Offloading & Tunnels")
                GTagPill(battLabel, color: powerStatus.onBattery ? G.amber : G.mint)
                Spacer()
                if savingOffload {
                    GSpinner(size: 11)
                }
            }

            Text("Automatically burst heavy builds, cross-arch (`amd64`) jobs, and Compose stacks to a paired Mac when battery falls below the threshold.")
                .font(.system(size: 12.5))
                .foregroundStyle(G.textMuted)

            Toggle("Enable burst offloading", isOn: Binding(
                get: { offloadPolicy.enabled },
                set: { val in offloadPolicy.enabled = val; Task { await saveOffloadPolicy() } }
            ))
            .toggleStyle(.gSwitch)
            .disabled(!store.canManage)

            HStack(spacing: 12) {
                Text("Preferred Mac")
                    .font(.system(size: 12.5))
                    .foregroundStyle(G.textMuted)
                    .frame(width: 110, alignment: .leading)
                Picker("Preferred Mac", selection: Binding(
                    get: { offloadPolicy.preferredHost ?? "" },
                    set: { val in
                        offloadPolicy.preferredHost = val.isEmpty ? nil : val
                        Task { await saveOffloadPolicy() }
                    }
                )) {
                    Text("None configured").tag("")
                    ForEach(hosts.hosts) { mac in
                        Text("\(mac.name) (\(mac.address))").tag(mac.name)
                    }
                }
                .labelsHidden()
                .disabled(!store.canManage)
            }

            HStack(spacing: 12) {
                Text("Battery threshold")
                    .font(.system(size: 12.5))
                    .foregroundStyle(G.textMuted)
                    .frame(width: 110, alignment: .leading)
                Slider(
                    value: Binding(
                        get: { Double(offloadPolicy.batteryThreshold) },
                        set: { offloadPolicy.batteryThreshold = UInt8($0.rounded()) }
                    ),
                    in: 15...90,
                    step: 5
                ) {
                    EmptyView()
                } onEditingChanged: { editing in
                    if !editing { Task { await saveOffloadPolicy() } }
                }
                .disabled(!store.canManage)
                Text("≤ \(offloadPolicy.batteryThreshold)%")
                    .font(G.mono(12))
                    .foregroundStyle(G.textPrimary)
                    .frame(width: 52, alignment: .trailing)
            }

            HStack(spacing: 18) {
                Toggle("Builds", isOn: Binding(
                    get: { offloadPolicy.offloadBuilds },
                    set: { val in offloadPolicy.offloadBuilds = val; Task { await saveOffloadPolicy() } }
                ))
                Toggle("Compose stacks", isOn: Binding(
                    get: { offloadPolicy.offloadCompose },
                    set: { val in offloadPolicy.offloadCompose = val; Task { await saveOffloadPolicy() } }
                ))
                Toggle("Cross-arch (amd64)", isOn: Binding(
                    get: { offloadPolicy.offloadMultiArch },
                    set: { val in offloadPolicy.offloadMultiArch = val; Task { await saveOffloadPolicy() } }
                ))
            }
            .toggleStyle(.checkbox)
            .disabled(!store.canManage)

            if !activeTunnels.isEmpty {
                Divider()
                GCaption("Active Migration Port Tunnels")
                VStack(spacing: 6) {
                    ForEach(activeTunnels) { tun in
                        HStack(spacing: 10) {
                            Image(systemName: "arrow.left.arrow.right")
                                .font(.system(size: 12))
                                .foregroundStyle(G.sky)
                            Text(tun.containerName)
                                .font(.system(size: 12.5, weight: .medium))
                            Text("127.0.0.1:\(tun.localPort) → \(tun.remoteHost):\(tun.remotePort)")
                                .font(G.mono(11.5))
                                .foregroundStyle(G.textMuted)
                            Spacer()
                            Button("Stop") {
                                Task {
                                    _ = try? await store.stopPortTunnel(id: tun.id)
                                    await loadOffloadAndTunnels()
                                }
                            }
                            .buttonStyle(.g(.danger, size: .small))
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .background(G.bg, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                    }
                }
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }

    private func loadOffloadAndTunnels() async {
        if let res = try? await store.powerAndOffload() {
            powerStatus = res.power
            offloadPolicy = res.policy
        }
        if let tuns = try? await store.listPortTunnels() {
            activeTunnels = tuns
        }
    }

    private func saveOffloadPolicy() async {
        savingOffload = true
        defer { savingOffload = false }
        if let res = try? await store.setOffloadPolicy(offloadPolicy) {
            powerStatus = res.power
            offloadPolicy = res.policy
        }
    }

    private var updatesCard: some View {
        let checking = updates.state == .checking
        return VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                GCaption("Updates")
                GTagPill("v\(updates.currentVersion)", color: G.textSecondary)
                Spacer()
                Button {
                    Task { await updates.checkNow(userInitiated: true) }
                } label: {
                    if checking {
                        HStack(spacing: 6) {
                            GSpinner(size: 11)
                            Text("Checking…")
                        }
                    } else {
                        Label("Check for Updates", systemImage: "arrow.triangle.2.circlepath")
                    }
                }
                .buttonStyle(.g(.secondary, size: .small))
                .disabled(checking)
            }

            Toggle(
                "Check for updates automatically",
                isOn: Binding(
                    get: { updates.autoCheckEnabled },
                    set: { updates.autoCheckEnabled = $0 }
                )
            )
            .toggleStyle(.gSwitch)

            if let checkedAt = updates.lastCheckedAt {
                Text("Last checked \(checkedAt.formatted(date: .abbreviated, time: .shortened)) against GitHub Releases.")
                    .font(.system(size: 12))
                    .foregroundStyle(G.textTertiary)
            } else {
                Text("Checks GitHub Releases (razeenwasif/Graviton) for new signed DMG releases.")
                    .font(.system(size: 12))
                    .foregroundStyle(G.textTertiary)
            }

            switch updates.state {
            case .idle, .checking:
                EmptyView()
            case let .upToDate(current, _):
                GStatusLine(.success, "Graviton v\(current) is up to date.")
            case let .available(info):
                let skipped = updates.skippedVersion == info.version.description
                VStack(alignment: .leading, spacing: 10) {
                    HStack(spacing: 8) {
                        Text(info.title)
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(G.textPrimary)
                        GTagPill("v\(info.version)", color: G.indigo)
                        if skipped {
                            GTagPill("Skipped", color: G.textMuted)
                        }
                        Spacer()
                    }
                    if !info.notes.isEmpty {
                        Text(Self.trimmedReleaseNotes(info.notes))
                            .font(.system(size: 12))
                            .foregroundStyle(G.textSecondary)
                            .lineLimit(4)
                    }
                    HStack(spacing: 8) {
                        Button("Download DMG", systemImage: "arrow.down.circle") {
                            NSWorkspace.shared.open(info.preferredDownloadURL)
                        }
                        .buttonStyle(.g(.primary, size: .small))

                        Button("Release Page", systemImage: "safari") {
                            NSWorkspace.shared.open(info.releaseURL)
                        }
                        .buttonStyle(.g(.secondary, size: .small))

                        Spacer()

                        if skipped {
                            Button("Clear Skip") { updates.clearSkipped() }
                                .buttonStyle(.g(.ghost, size: .small))
                        } else {
                            Button("Skip Version") { updates.skip(info) }
                                .buttonStyle(.g(.ghost, size: .small))
                        }
                    }
                }
                .padding(14)
                .background(G.indigo.opacity(0.10), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            case let .unavailable(message):
                GStatusLine(.info, message)
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }

    private static func trimmedReleaseNotes(_ notes: String) -> String {
        let condensed = notes
            .split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .prefix(4)
            .joined(separator: "\n")
        return condensed.count > 260 ? String(condensed.prefix(257)) + "…" : condensed
    }

    private var generalCard: some View {
        VStack(alignment: .leading, spacing: 14) {
            GCaption("General & Runtime")

            Toggle("Show Graviton in menu bar", isOn: $showsMenuBarExtra)
                .toggleStyle(.gSwitch)

            Toggle("Show running container count in menu bar", isOn: $menuBarShowsCount)
                .toggleStyle(.gSwitch)
                .disabled(!showsMenuBarExtra)

            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 10) {
                GridRow(alignment: .firstTextBaseline) {
                    Text("Active host").foregroundStyle(G.textMuted).frame(width: 150, alignment: .leading)
                    Text(hostLabel).font(G.mono(12)).foregroundStyle(G.body)
                }
                if let e = store.engine {
                    GridRow(alignment: .firstTextBaseline) {
                        Text("Guest kernel").foregroundStyle(G.textMuted).frame(width: 150, alignment: .leading)
                        Text(e.kernel).font(G.mono(12)).foregroundStyle(G.body)
                    }
                    GridRow(alignment: .firstTextBaseline) {
                        Text("Rosetta (amd64)").foregroundStyle(G.textMuted).frame(width: 150, alignment: .leading)
                        Text(e.rosetta == true ? "Available" : "Not installed")
                            .font(G.mono(12))
                            .foregroundStyle(e.rosetta == true ? G.mint : G.textMuted)
                    }
                }
                if store.isLocal {
                    GridRow(alignment: .center) {
                        Text("Docker CLI bridge").foregroundStyle(G.textMuted).frame(width: 150, alignment: .leading)
                        HStack(spacing: 10) {
                            Text("eval \"$(grav docker-env)\"")
                                .font(G.mono(12))
                                .foregroundStyle(G.textSecondary)
                                .textSelection(.enabled)
                            Button(copiedDockerEnv ? "Copied" : "Copy", systemImage: copiedDockerEnv ? "checkmark" : "doc.on.doc") {
                                NSPasteboard.general.clearContents()
                                NSPasteboard.general.setString("eval \"$(grav docker-env)\"", forType: .string)
                                copiedDockerEnv = true
                                Task {
                                    try? await Task.sleep(for: .seconds(2))
                                    copiedDockerEnv = false
                                }
                            }
                            .buttonStyle(.g(.secondary, size: .small))
                        }
                    }
                }
            }
            .padding(.top, 6)
            .font(.system(size: 13))
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }

    private var aboutCard: some View {
        HStack(spacing: 14) {
            GravitonMark(size: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text("Graviton")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(G.textPrimary)
                Text("Native Apple-silicon micro-VM container runtime · Version \(updates.currentVersion)")
                    .font(.system(size: 12))
                    .foregroundStyle(G.textMuted)
            }
            Spacer()
        }
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard(G.surfaceSubtle)
    }

    private func confirmLogout(_ login: RegistryLoginInfo) {
        dialogs.confirm(
            "Sign out of \(login.registry)?",
            message: "This removes \(login.username) and deletes the saved secret from \(hostLabel)'s login Keychain.",
            confirm: "Sign Out"
        ) {
            busyRegistry = login.registry
            statusBanner = nil
            Task {
                defer { busyRegistry = nil }
                do {
                    let removed = try await store.registryLogout(registry: login.registry)
                    statusBanner = (.success, "Signed out of \(removed).")
                } catch {
                    statusBanner = (.failure, error.localizedDescription)
                }
            }
        }
    }
}

struct RegistryLoginSheet: View {
    @Environment(EngineStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    var onSignedIn: (RegistryLoginInfo) -> Void

    @State private var registry = "docker.io"
    @State private var username = ""
    @State private var password = ""
    @State private var revealPassword = false
    @State private var busy = false
    @State private var error: String?

    private let quickRegistries = ["docker.io", "ghcr.io", "gcr.io", "quay.io"]
    private var canSubmit: Bool {
        !busy
            && !username.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !password.isEmpty
    }

    var body: some View {
        GSheet(
            "Sign In to Registry",
            subtitle: "Credentials are verified live against the registry and saved in Keychain on \(store.liveName ?? "This Mac").",
            symbol: "key.fill"
        ) {
            GFormRow("Registry", hint: "Pick a preset or enter a custom registry host (e.g. localhost:5000).") {
                VStack(alignment: .leading, spacing: 8) {
                    HStack(spacing: 6) {
                        ForEach(quickRegistries, id: \.self) { host in
                            Button(host) { registry = host }
                                .buttonStyle(.g(registry == host ? .primary : .secondary, size: .small))
                                .disabled(busy)
                        }
                    }
                    GTextField("docker.io", text: $registry)
                        .disabled(busy)
                        .gravitonField(mono: true)
                }
            }

            GFormRow("Username") {
                GTextField("username", text: $username)
                    .disabled(busy)
                    .gravitonField(mono: true)
            }

            GFormRow("Password or Personal Access Token") {
                HStack(spacing: 8) {
                    Group {
                        if revealPassword {
                            GTextField("token or password", text: $password)
                        } else {
                            GSecureField("token or password", text: $password)
                        }
                    }
                    .disabled(busy)

                    Button {
                        revealPassword.toggle()
                    } label: {
                        Image(systemName: revealPassword ? "eye.slash" : "eye")
                            .font(.system(size: 12))
                            .foregroundStyle(G.textMuted)
                    }
                    .buttonStyle(.plain)
                    .help(revealPassword ? "Hide secret" : "Show secret")
                }
                .gravitonField(mono: true)
            }

            if busy {
                GStatusLine(.working, "Verifying credentials with \(registry.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "docker.io" : registry)…")
            } else if let error {
                GStatusLine(.failure, error)
            }
        } footer: {
            Spacer()
            Button("Cancel") { dismiss() }
                .buttonStyle(.gSecondary)
                .keyboardShortcut(.cancelAction)
            Button("Sign In", action: signIn)
                .buttonStyle(.gPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(!canSubmit)
        }
    }

    private func signIn() {
        let targetRegistry = registry.trimmingCharacters(in: .whitespacesAndNewlines)
        let effectiveRegistry = targetRegistry.isEmpty ? "docker.io" : targetRegistry
        let user = username.trimmingCharacters(in: .whitespacesAndNewlines)
        busy = true
        error = nil
        Task {
            defer { busy = false }
            do {
                let info = try await store.registryLogin(registry: effectiveRegistry, username: user, password: password)
                onSignedIn(info)
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}
