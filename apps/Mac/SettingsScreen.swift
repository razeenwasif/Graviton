import AppKit
import GravitonKit
import SwiftUI

struct SettingsScreen: View {
    @Environment(EngineStore.self) private var store
    @Environment(Dialogs.self) private var dialogs
    @Environment(UpdateChecker.self) private var updates
    @AppStorage("showsMenuBarExtra") private var showsMenuBarExtra = true
    @AppStorage("menuBarShowsCount") private var menuBarShowsCount = true
    @State private var showLogin = false
    @State private var busyRegistry: String?
    @State private var statusBanner: (tone: GStatusLine.Tone, text: String)?
    @State private var copiedDockerEnv = false

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
                VStack(alignment: .leading, spacing: 18) {
                    if let banner = statusBanner {
                        GStatusLine(banner.tone, banner.text)
                    }

                    loginsCard
                    updatesCard
                    generalCard
                }
                .frame(maxWidth: 820, alignment: .leading)
                .padding(.horizontal, 28)
                .padding(.vertical, 16)
            }
        }
        .background(G.background)
        .task(id: "\(hostLabel)-\(store.connection == .live)") {
            await store.loadRegistryLogins()
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
                    GBadge("\(store.registryLogins.count)", color: G.accent)
                }
                Spacer()
            }
            Text("Usernames are stored in registries.json (mode 0600) on \(hostLabel); passwords and tokens live in \(store.isLocal ? "this Mac's" : "\(hostLabel)'s") login Keychain under service dev.graviton.registry.")
                .font(.system(size: 12.5))
                .foregroundStyle(G.muted)

            if store.registryLogins.isEmpty {
                HStack(spacing: 12) {
                    Image(systemName: "key")
                        .font(.system(size: 16))
                        .foregroundStyle(G.faint)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("No saved registry logins")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(G.textSecondary)
                        Text("Sign in to push images or pull private repositories.")
                            .font(.system(size: 12))
                            .foregroundStyle(G.faint)
                    }
                    Spacer()
                    Button("Sign In…") { showLogin = true }
                        .buttonStyle(.g(.secondary, size: .small))
                        .disabled(!store.canManage)
                }
                .padding(14)
                .background(G.background.opacity(0.6), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.border))
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(store.registryLogins.enumerated()), id: \.element.id) { idx, login in
                        if idx > 0 {
                            Rectangle().fill(G.border).frame(height: 1)
                        }
                        HStack(spacing: 12) {
                            Image(systemName: "lock.shield")
                                .font(.system(size: 14))
                                .foregroundStyle(G.accent)
                                .frame(width: 24)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(login.registry)
                                    .font(G.mono(13, weight: .medium))
                                    .foregroundStyle(G.text)
                                Text(login.username)
                                    .font(G.mono(11.5))
                                    .foregroundStyle(G.muted)
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
                    }
                }
                .background(G.background.opacity(0.6), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.border))
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard()
    }

    private var updatesCard: some View {
        let checking = updates.state == .checking
        return VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                GCaption("Updates")
                GBadge("v\(updates.currentVersion)", color: G.muted)
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
                    .foregroundStyle(G.faint)
            } else {
                Text("Checks GitHub Releases (razeenwasif/Graviton) for new signed DMG releases.")
                    .font(.system(size: 12))
                    .foregroundStyle(G.faint)
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
                            .foregroundStyle(G.text)
                        GBadge("v\(info.version)", color: G.accent)
                        if skipped {
                            GBadge("Skipped", color: G.muted)
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
                .background(G.accent.opacity(0.08), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(G.accent.opacity(0.28)))
            case let .unavailable(message):
                GStatusLine(.info, message)
            case let .failed(message):
                GStatusLine(.failure, message)
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard()
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

            Rectangle().fill(G.border).frame(height: 1)

            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 8) {
                GridRow(alignment: .firstTextBaseline) {
                    Text("Active host").foregroundStyle(G.muted).frame(width: 150, alignment: .leading)
                    Text(hostLabel).font(G.mono(12))
                }
                if let e = store.engine {
                    GridRow(alignment: .firstTextBaseline) {
                        Text("Guest kernel").foregroundStyle(G.muted).frame(width: 150, alignment: .leading)
                        Text(e.kernel).font(G.mono(12))
                    }
                    GridRow(alignment: .firstTextBaseline) {
                        Text("Rosetta (amd64)").foregroundStyle(G.muted).frame(width: 150, alignment: .leading)
                        Text(e.rosetta == true ? "Available" : "Not installed").font(G.mono(12))
                            .foregroundStyle(e.rosetta == true ? G.running : G.muted)
                    }
                }
                if store.isLocal {
                    GridRow(alignment: .center) {
                        Text("Docker CLI bridge").foregroundStyle(G.muted).frame(width: 150, alignment: .leading)
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
            .font(.system(size: 13))
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
        .gravitonCard()
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
                            .foregroundStyle(G.muted)
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
